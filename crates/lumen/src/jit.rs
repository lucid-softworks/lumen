//! ARM64 template JIT (macOS/Apple Silicon): the third execution tier.
//!
//! A compiled [`crate::bytecode::Chunk`] lowers to machine code one bytecode op at a time. Most
//! ops become a call into [`crate::bytecode::jit_exec`] — the single slow-path helper that runs
//! exactly one op against a raw operand-stack pointer — with the op index baked in as an
//! immediate. Control flow (jumps, conditional branches, returns, try/catch) is real machine
//! branches between per-op labels, so the interpreter's fetch/dispatch loop disappears entirely.
//! Hot ops gain inline fast paths over the templates in later passes.
//!
//! The operand stack is a pre-sized flat buffer (its maximum depth is computed statically from
//! the op stream), held in a callee-saved register; helpers return the new stack top, or null to
//! signal a throw, which routes through a shared unwind block that consults the try-handler
//! stack recorded by `PushHandler` templates.
//!
//! Everything is `cfg`-gated to aarch64 + macOS; elsewhere `compile` returns `None` and the
//! bytecode VM runs as before.

#![cfg_attr(not(all(target_arch = "aarch64", target_os = "macos")), allow(dead_code))]

use std::rc::Rc;

use crate::bytecode::Chunk;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

// ---------------------------------------------------------------------------------------------
// Executable memory (macOS MAP_JIT)
// ---------------------------------------------------------------------------------------------

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod sys {
    extern "C" {
        pub fn mmap(
            addr: *mut u8,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut u8;
        pub fn munmap(addr: *mut u8, len: usize) -> i32;
        pub fn pthread_jit_write_protect_np(enabled: i32);
        pub fn sys_icache_invalidate(start: *mut u8, len: usize);
    }
    pub const PROT_RWX: i32 = 0x1 | 0x2 | 0x4;
    pub const MAP_PRIVATE_ANON_JIT: i32 = 0x0002 | 0x1000 | 0x0800;
}

/// A finished JIT compilation: executable code plus the pc→code-offset table the unwinder uses
/// to land on catch handlers.
pub struct JitCode {
    mem: *mut u8,
    len: usize,
    /// Code byte offset of each bytecode pc (catch targets and branch targets).
    pc_offsets: Vec<u32>,
    /// Statically computed maximum operand-stack depth.
    pub max_stack: usize,
}

impl Drop for JitCode {
    fn drop(&mut self) {
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        unsafe {
            sys::munmap(self.mem, self.len);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The runtime context shared between JIT code and its Rust helpers
// ---------------------------------------------------------------------------------------------

/// Passed to the JIT entry in x0. The leading fields are read from assembly by fixed offset —
/// keep their order in sync with the prologue/epilogue emitters below. Everything after is only
/// touched from Rust helpers.
#[repr(C)]
pub struct JitCtx {
    /// [0] Helper function table (see `HELPER_*` indices).
    pub helpers: *const usize,
    /// [8] Operand-stack base; the JIT keeps the live top in a register and stores it back here
    /// on every exit path.
    pub stack_base: *mut Value,
    /// [16] Final stack top, written by the epilogues (for leftover-value cleanup on throw).
    pub final_sp: *mut Value,
    /// [24] Local slots base (the inline LoadLocal/StoreLocal templates index off this).
    pub slots: *mut Value,
    /// [32] Points at `Interp::inline_ic_safe` (a `Cell<bool>` byte): the inline property-cache
    /// templates read it live and fall to the helper when it is 0.
    pub inline_ic_safe: *const u8,
    // ---- Rust-only fields ----
    pub interp: *mut Interp,
    pub chunk: *const Chunk,
    pub env: Env,
    pub this_val: Value,
    pub n_slots: usize,
    /// Active `try` regions: (catch pc, operand-stack depth to unwind to).
    pub handlers: Vec<(u32, usize)>,
    pub code_base: *const u8,
    pub pc_offsets: *const u32,
    pub error: Option<Abrupt>,
    pub ret: Value,
}

/// Helper table indices (multiplied by 8 in the emitted `ldr`).
pub const H_EXEC: usize = 0;
pub const H_COND: usize = 1;
pub const H_RETURN: usize = 2;
pub const H_PUSH_HANDLER: usize = 3;
pub const H_POP_HANDLER: usize = 4;
pub const H_UNWIND: usize = 5;
pub const N_HELPERS: usize = 6;

/// ARM64 condition codes used by the inline templates.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_EQ: u32 = 0;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_NE: u32 = 1;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_LO: u32 = 3;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_MI: u32 = 4;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_HI: u32 = 8;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_LS: u32 = 9;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_GE: u32 = 10;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_GT: u32 = 12;

/// Condition-helper modes (the `w1` immediate for `H_COND`).
pub const COND_POP_TRUTHY: u32 = 0;
pub const COND_PEEK_TRUTHY: u32 = 1;
pub const COND_PEEK_NOT_NULLISH: u32 = 2;

// The inline fast paths read Value directly: repr(u8) tag byte at offset 0, payload at
// offset 8, 24 bytes total on 64-bit. Tags 0..=4 (Undefined/Empty/Null/Bool/Num) are trivially
// copyable. Only the JIT (aarch64-macos) depends on this, so the layout is only asserted there —
// on a 32-bit target (wasm) `Value` is smaller and the JIT does not exist.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const _: () = assert!(std::mem::size_of::<Value>() == 24);
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const _: () = assert!(std::mem::align_of::<Value>() == 8);

/// Two-register return for helpers that produce (new sp, flag) — x0/x1 under the C ABI.
#[repr(C)]
pub struct SpFlag {
    pub sp: *mut Value,
    pub flag: u64,
}

// ---------------------------------------------------------------------------------------------
// ARM64 assembler (the ~20 encodings the templates need)
// ---------------------------------------------------------------------------------------------

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod asm {
    /// Instruction buffer with label/patch support. Registers are plain u32 numbers (x0..x30,
    /// sp=31 where encodable); labels are indices into `patches`.
    pub struct Asm {
        pub buf: Vec<u32>,
        /// (instruction index, label id, kind) — resolved in `finish`.
        patches: Vec<(usize, usize, PatchKind)>,
        labels: Vec<Option<usize>>, // label id → instruction index
    }

    #[derive(Clone, Copy)]
    enum PatchKind {
        /// Unconditional B: imm26.
        B,
        /// CBZ/CBNZ: imm19.
        Cb,
    }

    impl Asm {
        pub fn new() -> Asm {
            Asm {
                buf: Vec::new(),
                patches: Vec::new(),
                labels: Vec::new(),
            }
        }
        pub fn here(&self) -> usize {
            self.buf.len()
        }
        pub fn new_label(&mut self) -> usize {
            self.labels.push(None);
            self.labels.len() - 1
        }
        pub fn bind(&mut self, label: usize) {
            self.labels[label] = Some(self.buf.len());
        }
        fn emit(&mut self, i: u32) {
            self.buf.push(i);
        }

        /// movz xd, #imm16, lsl #(shift*16)
        pub fn movz(&mut self, rd: u32, imm16: u32, shift: u32) {
            self.emit(0xD280_0000 | (shift << 21) | (imm16 << 5) | rd);
        }
        /// movk xd, #imm16, lsl #(shift*16)
        #[allow(dead_code)] // the inline fast-path pass uses these
        pub fn movk(&mut self, rd: u32, imm16: u32, shift: u32) {
            self.emit(0xF280_0000 | (shift << 21) | (imm16 << 5) | rd);
        }
        /// mov xd, xn (ORR xd, xzr, xn)
        pub fn mov(&mut self, rd: u32, rn: u32) {
            self.emit(0xAA00_03E0 | (rn << 16) | rd);
        }
        /// Load a 64-bit constant via movz/movk chain.
        #[allow(dead_code)]
        pub fn mov_imm64(&mut self, rd: u32, v: u64) {
            self.movz(rd, (v & 0xffff) as u32, 0);
            if (v >> 16) & 0xffff != 0 || v >> 16 != 0 {
                self.movk(rd, ((v >> 16) & 0xffff) as u32, 1);
            }
            if v >> 32 != 0 {
                self.movk(rd, ((v >> 32) & 0xffff) as u32, 2);
            }
            if v >> 48 != 0 {
                self.movk(rd, ((v >> 48) & 0xffff) as u32, 3);
            }
        }
        /// ldr xd, [xn, #imm] (imm = byte offset, multiple of 8, unsigned)
        pub fn ldr_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes % 8 == 0 && imm_bytes / 8 < 4096);
            self.emit(0xF940_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// str xt, [xn, #imm]
        pub fn str_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes % 8 == 0 && imm_bytes / 8 < 4096);
            self.emit(0xF900_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// stp xt1, xt2, [sp, #-imm]! (pre-index, imm = positive byte count, multiple of 8)
        pub fn stp_pre(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA980_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// ldp xt1, xt2, [sp], #imm (post-index)
        pub fn ldp_post(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA8C0_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// stp xt1, xt2, [sp, #imm] (signed offset form)
        pub fn stp_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA900_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// ldp xt1, xt2, [sp, #imm]
        pub fn ldp_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA940_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        pub fn blr(&mut self, rn: u32) {
            self.emit(0xD63F_0000 | (rn << 5));
        }
        pub fn br(&mut self, rn: u32) {
            self.emit(0xD61F_0000 | (rn << 5));
        }
        pub fn ret(&mut self) {
            self.emit(0xD65F_03C0);
        }
        /// b label (patched)
        pub fn b(&mut self, label: usize) {
            self.patches.push((self.buf.len(), label, PatchKind::B));
            self.emit(0x1400_0000);
        }
        /// cbz x/w reg, label (patched); `is64` selects X vs W.
        pub fn cbz(&mut self, rt: u32, is64: bool, label: usize) {
            self.patches.push((self.buf.len(), label, PatchKind::Cb));
            self.emit(if is64 { 0xB400_0000 } else { 0x3400_0000 } | rt);
        }
        /// cbnz x/w reg, label (patched)
        pub fn cbnz(&mut self, rt: u32, is64: bool, label: usize) {
            self.patches.push((self.buf.len(), label, PatchKind::Cb));
            self.emit(if is64 { 0xB500_0000 } else { 0x3500_0000 } | rt);
        }

        /// ldrb wt, [xn, #imm] (unsigned byte offset)
        pub fn ldrb_imm(&mut self, rt: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x3940_0000 | (imm << 10) | (rn << 5) | rt);
        }
        /// strb wt, [xn, #imm]
        #[allow(dead_code)]
        pub fn strb_imm(&mut self, rt: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x3900_0000 | (imm << 10) | (rn << 5) | rt);
        }
        /// sturb wt, [xn, #simm9]
        pub fn sturb(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0x3800_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// ldurb wt, [xn, #simm9]
        pub fn ldurb(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0x3840_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// ldur xt, [xn, #simm9]
        pub fn ldur(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0xF840_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// stur xt, [xn, #simm9]
        pub fn stur(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0xF800_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// ldr wt, [xn, #imm] (32-bit, unsigned scaled by 4)
        pub fn ldr_w_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes % 4 == 0 && imm_bytes / 4 < 4096);
            self.emit(0xB940_0000 | ((imm_bytes / 4) << 10) | (rn << 5) | rt);
        }
        /// madd xd, xn, xm, xa  (xd = xn*xm + xa)
        pub fn madd(&mut self, rd: u32, rn: u32, rm: u32, ra: u32) {
            self.emit(0x9B00_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd);
        }
        /// cmp wn, wm  (SUBS wzr, wn, wm)
        pub fn cmp_reg_w(&mut self, rn: u32, rm: u32) {
            self.emit(0x6B00_001F | (rm << 16) | (rn << 5));
        }
        /// cmp xn, #imm12
        pub fn cmp_imm_x(&mut self, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0xF100_001F | (imm << 10) | (rn << 5));
        }
        /// ldur dt, [xn, #simm9]
        pub fn ldur_d(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0xFC40_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// stur dt, [xn, #simm9]
        pub fn stur_d(&mut self, rt: u32, rn: u32, simm9: i32) {
            self.emit(0xFC00_0000 | (((simm9 as u32) & 0x1FF) << 12) | (rn << 5) | rt);
        }
        /// ldr dt, [xn, #imm] (scaled)
        pub fn ldr_d_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes % 8 == 0 && imm_bytes / 8 < 4096);
            self.emit(0xFD40_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// str dt, [xn, #imm] (scaled)
        pub fn str_d_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes % 8 == 0 && imm_bytes / 8 < 4096);
            self.emit(0xFD00_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// add xd, xn, #imm12
        pub fn add_imm(&mut self, rd: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x9100_0000 | (imm << 10) | (rn << 5) | rd);
        }
        /// sub xd, xn, #imm12
        pub fn sub_imm(&mut self, rd: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0xD100_0000 | (imm << 10) | (rn << 5) | rd);
        }
        /// cmp wn, #imm12
        pub fn cmp_imm_w(&mut self, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x7100_001F | (imm << 10) | (rn << 5));
        }
        /// b.cond label (patched; imm19 shares the CBZ patch shape)
        pub fn b_cond(&mut self, cond: u32, label: usize) {
            self.patches.push((self.buf.len(), label, PatchKind::Cb));
            self.emit(0x5400_0000 | cond);
        }
        /// fadd/fsub/fmul/fdiv dd, dn, dm — op: 0=add,1=sub,2=mul,3=div
        pub fn f_arith(&mut self, op: u32, rd: u32, rn: u32, rm: u32) {
            let bits = match op {
                0 => 0x1E60_2800u32,
                1 => 0x1E60_3800,
                2 => 0x1E60_0800,
                _ => 0x1E60_1800,
            };
            self.emit(bits | (rm << 16) | (rn << 5) | rd);
        }
        /// fcmp dn, dm
        pub fn fcmp(&mut self, rn: u32, rm: u32) {
            self.emit(0x1E60_2000 | (rm << 16) | (rn << 5));
        }
        /// cset wd, cond (CSINC wd, wzr, wzr, !cond)
        pub fn cset_w(&mut self, rd: u32, cond: u32) {
            self.emit(0x1A9F_07E0 | ((cond ^ 1) << 12) | rd);
        }
        /// fmov dd, #1.0
        pub fn fmov_one(&mut self, rd: u32) {
            self.emit(0x1E6E_1000 | rd);
        }

        /// Resolve all label patches. Panics on an unbound label (a compiler bug).
        pub fn finish(mut self) -> Vec<u32> {
            for (at, label, kind) in std::mem::take(&mut self.patches) {
                let target = self.labels[label].expect("unbound jit label");
                let delta = target as i64 - at as i64; // in instructions
                match kind {
                    PatchKind::B => {
                        let imm26 = (delta as u32) & 0x03FF_FFFF;
                        self.buf[at] |= imm26;
                    }
                    PatchKind::Cb => {
                        let imm19 = ((delta as u32) & 0x7FFFF) << 5;
                        self.buf[at] |= imm19;
                    }
                }
            }
            self.buf
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------------------------

/// Compile `chunk` to machine code, or `None` when unsupported (non-macOS/ARM64, async bodies,
/// or an op stream whose stack depths don't line up — a compiler bug caught defensively).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub fn compile(chunk: &Chunk, layout: &crate::value::JitLayout) -> Option<JitCode> {
    use crate::bytecode::{Op, UpdKind};

    let ops = chunk.jit_ops();
    if ops.len() > 0xFFFF {
        return None; // op index must fit one movz
    }
    // Async bodies suspend; the VM's coroutine runs them.
    if ops.iter().any(|o| matches!(o, Op::Await)) {
        return None;
    }
    let max_stack = stack_depths(chunk)?;
    let fast: u32 = std::env::var("LUMEN_JIT_FAST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);

    let mut a = asm::Asm::new();
    // One label per bytecode pc (branch/catch targets bind as we emit).
    let pc_labels: Vec<usize> = (0..ops.len()).map(|_| a.new_label()).collect();
    let l_unwind = a.new_label();
    let l_ret_ok = a.new_label();
    let l_ret_throw = a.new_label();

    // ---- prologue ----
    // Frame: save fp/lr + x19..x22 (we use x19=ctx, x20=sp, x21=helpers).
    a.stp_pre(29, 30, -48);
    a.stp_off(19, 20, 16);
    a.stp_off(21, 22, 32);
    a.mov(19, 0); // ctx
    a.ldr_imm(21, 19, 0); // helpers table
    a.ldr_imm(20, 19, 8); // sp = stack_base
    a.ldr_imm(22, 19, 24); // local slots base

    // ---- op templates ----
    let mut pc_insn: Vec<u32> = Vec::with_capacity(ops.len());
    for (pc, op) in ops.iter().enumerate() {
        a.bind(pc_labels[pc]);
        pc_insn.push(a.here() as u32);
        match op {
            Op::Jump(t) => {
                a.b(pc_labels[*t as usize]);
            }
            Op::JumpIfFalse(t) if fast & 4 != 0 => {
                // Bool on top (the compare fast paths produce one): branch on its payload byte.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 3);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -23); // bool payload at offset 1
                a.sub_imm(20, 20, 24);
                a.cbz(9, false, pc_labels[*t as usize]);
                a.b(done);
                a.bind(slow);
                emit_cond(&mut a, COND_POP_TRUTHY, l_unwind);
                a.cbz(1, false, pc_labels[*t as usize]);
                a.bind(done);
            }
            Op::JumpIfFalse(t) => {
                emit_cond(&mut a, COND_POP_TRUTHY, l_unwind);
                a.cbz(1, false, pc_labels[*t as usize]);
            }
            Op::JumpIfFalsePeek(t) => {
                emit_cond(&mut a, COND_PEEK_TRUTHY, l_unwind);
                a.cbz(1, false, pc_labels[*t as usize]);
            }
            Op::JumpIfTruePeek(t) => {
                emit_cond(&mut a, COND_PEEK_TRUTHY, l_unwind);
                a.cbnz(1, false, pc_labels[*t as usize]);
            }
            Op::JumpIfNotNullishPeek(t) => {
                emit_cond(&mut a, COND_PEEK_NOT_NULLISH, l_unwind);
                a.cbnz(1, false, pc_labels[*t as usize]);
            }
            Op::Return => {
                emit_helper(&mut a, H_RETURN, 1);
                a.b(l_ret_ok);
            }
            Op::ReturnUndef => {
                emit_helper(&mut a, H_RETURN, 0);
                a.b(l_ret_ok);
            }
            Op::PushHandler(t) => {
                emit_helper(&mut a, H_PUSH_HANDLER, *t);
            }
            Op::PopHandler => {
                emit_helper(&mut a, H_POP_HANDLER, 0);
            }
            Op::Throw => {
                // The generic executor sets ctx.error and returns null.
                emit_exec(&mut a, pc as u32, l_unwind);
            }
            Op::Await => unreachable!("async chunks are rejected above"),
            // ---- inline property cache: own-property read (`this.x`) ----
            Op::GetProp(_, cache) if fast & 256 != 0 && get_prop_inlinable(layout) => {
                emit_get_prop_inline(&mut a, layout, chunk.jit_cache_ptr(*cache), pc as u32, l_unwind);
            }
            // ---- inline property cache: method load (`obj.m(...)`) ----
            Op::GetMethod(_, cache) if fast & 512 != 0 && get_method_inlinable(layout) => {
                emit_get_method_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                );
            }
            // ---- inline fast paths (tags: 3 = Bool, 4 = Num; payload at +8; Value = 24) ----
            Op::Add | Op::Sub | Op::Mul | Op::Div if fast & 1 != 0 => {
                let f_op = match op {
                    Op::Add => 0,
                    Op::Sub => 1,
                    Op::Mul => 2,
                    _ => 3,
                };
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -48);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -40);
                a.ldur_d(1, 20, -16);
                a.f_arith(f_op, 0, 0, 1);
                a.stur_d(0, 20, -40);
                a.sub_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::StrictEq | Op::StrictNotEq | Op::EqEq
            | Op::NotEq
                if fast & 2 != 0 =>
            {
                // Number-number compare: FCMP + CSET with IEEE-correct conditions (unordered
                // yields false for the ordered relations, true only for !=).
                let cond = match op {
                    Op::Lt => C_MI,
                    Op::Gt => C_GT,
                    Op::Le => C_LS,
                    Op::Ge => C_GE,
                    Op::StrictEq | Op::EqEq => C_EQ,
                    _ => C_NE,
                };
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -48);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -40);
                a.ldur_d(1, 20, -16);
                a.fcmp(0, 1);
                a.cset_w(9, cond);
                a.movz(10, 3, 0); // Bool tag word (payload byte 1 zeroed by the 64-bit store)
                a.sub_imm(20, 20, 24);
                a.stur(10, 20, -24);
                a.sturb(9, 20, -23); // bool payload at offset 1
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::LoadLocal(slot) if fast & 8 != 0 && (*slot as u32) * 24 + 16 < 4096 => {
                let off = *slot as u32 * 24;
                let slow = a.new_label();
                let done = a.new_label();
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 1); // Empty = TDZ throw → slow
                a.b_cond(C_EQ, slow);
                a.cmp_imm_w(9, 4); // refcounted → slow (must clone)
                a.b_cond(C_HI, slow);
                a.ldr_imm(10, 22, off);
                a.ldr_imm(11, 22, off + 8);
                a.stur(10, 20, 0);
                a.stur(11, 20, 8);
                a.add_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::StoreLocal(slot) if fast & 16 != 0 && (*slot as u32) * 24 + 16 < 4096 => {
                let off = *slot as u32 * 24;
                let slow = a.new_label();
                let done = a.new_label();
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 4); // old value refcounted → slow (must drop)
                a.b_cond(C_HI, slow);
                // Move the popped value (all 24 bytes — a refcounted payload moves, not clones).
                a.ldur(9, 20, -24);
                a.ldur(10, 20, -16);
                a.ldur(11, 20, -8);
                a.str_imm(9, 22, off);
                a.str_imm(10, 22, off + 8);
                a.str_imm(11, 22, off + 16);
                a.sub_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::UpdateLocal(slot, kind) if fast & 32 != 0 && (*slot as u32) * 24 + 8 < 4096 => {
                let off = *slot as u32 * 24;
                let slow = a.new_label();
                let done = a.new_label();
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldr_d_imm(0, 22, off + 8); // old
                a.fmov_one(1);
                let dec = matches!(
                    kind,
                    UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
                );
                a.f_arith(if dec { 1 } else { 0 }, 2, 0, 1); // new = old ± 1
                a.str_d_imm(2, 22, off + 8);
                match kind {
                    UpdKind::PreInc | UpdKind::PreDec => {
                        a.movz(10, 4, 0);
                        a.stur(10, 20, 0);
                        a.stur_d(2, 20, 8);
                        a.add_imm(20, 20, 24);
                    }
                    UpdKind::PostInc | UpdKind::PostDec => {
                        a.movz(10, 4, 0);
                        a.stur(10, 20, 0);
                        a.stur_d(0, 20, 8);
                        a.add_imm(20, 20, 24);
                    }
                    UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                }
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Pop if fast & 64 != 0 => {
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_HI, slow); // refcounted → slow (must drop)
                a.sub_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Const(k) if fast & 128 != 0 && chunk.jit_const_copyable(*k) => {
                let (word0, word1) = chunk.jit_const_bits(*k);
                a.mov_imm64(9, word0);
                a.stur(9, 20, 0);
                a.mov_imm64(9, word1);
                a.stur(9, 20, 8);
                a.add_imm(20, 20, 24);
            }
            _ => {
                emit_exec(&mut a, pc as u32, l_unwind);
            }
        }
    }
    // Fall off the end: return undefined (compile() always terminates with ReturnUndef, but be
    // safe about it).
    emit_helper(&mut a, H_RETURN, 0);
    a.b(l_ret_ok);

    // ---- unwind: route a throw to the innermost try handler, or out ----
    a.bind(l_unwind);
    a.mov(0, 19);
    a.movz(1, 0, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (H_UNWIND * 8) as u32);
    a.blr(16);
    a.cbz(0, true, l_ret_throw);
    a.mov(20, 1);
    a.br(0);

    // ---- epilogues ----
    a.bind(l_ret_ok);
    a.str_imm(20, 19, 16); // ctx.final_sp = sp
    a.movz(0, 1, 0);
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    a.ldp_post(29, 30, 48);
    a.ret();
    a.bind(l_ret_throw);
    a.str_imm(20, 19, 16);
    a.movz(0, 0, 0);
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    a.ldp_post(29, 30, 48);
    a.ret();

    let words = a.finish();
    let len = words.len() * 4;
    unsafe {
        let mem = sys::mmap(
            std::ptr::null_mut(),
            len,
            sys::PROT_RWX,
            sys::MAP_PRIVATE_ANON_JIT,
            -1,
            0,
        );
        if mem as isize == -1 {
            return None;
        }
        sys::pthread_jit_write_protect_np(0);
        std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, mem, len);
        sys::pthread_jit_write_protect_np(1);
        sys::sys_icache_invalidate(mem, len);
        Some(JitCode {
            mem,
            len,
            pc_offsets: pc_insn.iter().map(|i| i * 4).collect(),
            max_stack,
        })
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
pub fn compile(_chunk: &Chunk, _layout: &crate::value::JitLayout) -> Option<JitCode> {
    None
}

/// Whether `layout` is usable for the inline GetProp template: valid (probed std layouts hold)
/// and every offset it bakes fits its instruction's immediate range.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn get_prop_inlinable(layout: &crate::value::JitLayout) -> bool {
    let sh = layout.obj_props + layout.props_shape;
    let en = layout.obj_props + layout.props_entries;
    layout.valid
        && layout.refcell_value < 4096
        && layout.obj_exotic < 4096
        && sh % 4 == 0
        && sh / 4 < 4096
        && en % 8 == 0
        && en / 8 < 4096
        && layout.entry_accessor < 4096
        && layout.entry_value + 16 < 256
        && layout.rc_strong_back < 256
        && layout.entry_size < 0x1_0000
}

/// Inline own-property read (`this.x`): validate the receiver by shape and load the value in
/// machine code, taking the checked helper on any mismatch. Every guard branches to `slow`
/// *before* the template writes any state, so the fallback re-runs the op cleanly. Handles only
/// a `depth == 0` hit on a non-exotic ordinary object whose receiver is not the last reference
/// (so the pop-drop can't free) and whose value is trivially copyable (tag ≤ 4, no refcount);
/// everything else — methods, refcounted values, proxies (via the live `inline_ic_safe` flag) —
/// falls through.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_get_prop_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
) {
    use crate::bytecode::{IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT};
    let rc_back = layout.rc_strong_back as i32;
    let rcv = layout.refcell_value as u32;
    let ex = layout.obj_exotic as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    let slow = a.new_label();
    let done = a.new_label();
    // 0. inline caches globally safe? (no proxy/typed-array/namespace exists)
    a.ldr_imm(9, 19, 32); // ctx.inline_ic_safe pointer
    a.ldrb_imm(9, 9, 0);
    a.cbz(9, false, slow);
    // 1. receiver must be an Obj (tag 8)
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -16); // rc_ptr (Value payload)
    // 2. receiver refcount > 1 (so the pop-drop below never frees)
    a.ldur(9, 10, -rc_back);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, slow);
    // 3. cache: depth must be 0, load slot + cached receiver shape
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.cbnz(9, false, slow);
    a.ldr_w_imm(13, 12, IC_OFF_SLOT);
    a.ldr_w_imm(14, 12, IC_OFF_RECV_SHAPE);
    // 4. object base; exotic must be None
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    // 5. shape id matches
    a.ldr_w_imm(9, 11, sh);
    a.cmp_reg_w(9, 14);
    a.b_cond(C_NE, slow);
    // 6. entry base = entries data ptr + slot*entry_size
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 7. not an accessor
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    // 8. value tag: a BigInt (5) is a compound payload — leave it to the helper. Everything else
    //    is either trivially copyable (≤4) or a single Rc pointer at value+8 (Str/Sym/Obj, 6..8).
    a.ldurb(9, 15, ev); // w9 = value tag (kept live through the loads below)
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    // --- commit: everything validated; from here only writes ---
    // load the 24-byte value (x9/tag untouched)
    a.ldur(12, 15, ev);
    a.ldur(13, 15, ev + 8); // payload word (the Rc pointer for tags 6..8)
    a.ldur(14, 15, ev + 16);
    // clone: a refcounted value (tag ≥ 6) needs its strong count bumped (payload − rc_back)
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 13, -rc_back);
    a.add_imm(16, 16, 1);
    a.stur(16, 13, -rc_back);
    a.bind(nobump);
    // drop the receiver (strong was > 1: decrement, no free). If the value IS the receiver the
    // bump above already balanced this.
    a.ldur(9, 10, -rc_back);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, -rc_back);
    // overwrite the receiver slot with the value (pop obj + push value = same depth)
    a.stur(12, 20, -24);
    a.stur(13, 20, -16);
    a.stur(14, 20, -8);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Same immediate-range gate as [`get_prop_inlinable`] plus the `proto` offset (GetMethod walks
/// one prototype hop).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn get_method_inlinable(layout: &crate::value::JitLayout) -> bool {
    get_prop_inlinable(layout) && layout.obj_proto < 4096
}

/// Inline method load (`obj.method(...)` → `GetMethod`): the receiver stays on the stack (it is
/// re-pushed as `this`), and the method — found one prototype hop up (`depth == 1`) — is loaded
/// and pushed above it. Validates the receiver *and* holder by shape; re-follows the live proto
/// so a proto swap misses. Only a non-exotic ordinary receiver with a non-exotic ordinary
/// immediate prototype holding a non-BigInt method at the cached slot inlines; anything else
/// falls to the helper. No receiver refcount change (it is neither dropped nor cloned — the same
/// stack Value serves as both operands); the method is cloned (bumped).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_get_method_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
) {
    use crate::bytecode::{IC_OFF_DEPTH, IC_OFF_HOLDER_SHAPE, IC_OFF_RECV_SHAPE, IC_OFF_SLOT};
    let rc_back = layout.rc_strong_back as i32;
    let rcv = layout.refcell_value as u32;
    let ex = layout.obj_exotic as u32;
    let pr = layout.obj_proto as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    let slow = a.new_label();
    let done = a.new_label();
    // 0. inline caches globally safe?
    a.ldr_imm(9, 19, 32);
    a.ldrb_imm(9, 9, 0);
    a.cbz(9, false, slow);
    // 1. receiver is an Obj
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -16); // receiver rc_ptr
    // 2. cache: depth must be 1; load slot, recv & holder shapes
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.cmp_imm_w(9, 1);
    a.b_cond(C_NE, slow);
    a.ldr_w_imm(13, 12, IC_OFF_SLOT); // slot
    // 3. receiver object: exotic None, shape == recv_shape
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    a.ldr_w_imm(9, 11, sh);
    a.ldr_w_imm(16, 12, IC_OFF_RECV_SHAPE);
    a.cmp_reg_w(9, 16);
    a.b_cond(C_NE, slow);
    // 4. follow proto (Option<Gc> niche: pointer or 0)
    a.ldr_imm(10, 11, pr); // proto rc_ptr (reuse x10 — receiver rc no longer needed)
    a.cbz(10, true, slow); // null proto → slow
    // 5. holder object: exotic None, shape == holder_shape
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    a.ldr_w_imm(9, 11, sh);
    a.ldr_w_imm(16, 12, IC_OFF_HOLDER_SHAPE);
    a.cmp_reg_w(9, 16);
    a.b_cond(C_NE, slow);
    // 6. entry base = holder.entries + slot*entry_size
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 7. not an accessor
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    // 8. method tag: BigInt (5) → helper
    a.ldurb(9, 15, ev);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    // --- commit: receiver stays at [-24]; push the method above it ---
    a.ldur(12, 15, ev);
    a.ldur(13, 15, ev + 8); // payload (Rc pointer for tags 6..8; methods are Obj)
    a.ldur(14, 15, ev + 16);
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 13, -rc_back);
    a.add_imm(16, 16, 1);
    a.stur(16, 13, -rc_back);
    a.bind(nobump);
    a.stur(12, 20, 0);
    a.stur(13, 20, 8);
    a.stur(14, 20, 16);
    a.add_imm(20, 20, 24); // pushed the method
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// The generic per-op helper call: `jit_exec(ctx, pc, sp)` → (new sp, threw?). The sp is taken
/// unconditionally — it reflects consumed operands even when the op threw, which is what keeps
/// the unwinder's cleanup from re-dropping moved-out slots.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_exec(a: &mut asm::Asm, pc: u32, l_unwind: usize) {
    a.mov(0, 19);
    a.movz(1, pc, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (H_EXEC * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
    a.cbnz(1, false, l_unwind);
}

/// An infallible helper (returns the new sp): return/handler bookkeeping.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_helper(a: &mut asm::Asm, idx: usize, imm: u32) {
    a.mov(0, 19);
    a.movz(1, imm, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (idx * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
}

/// Condition helper: leaves the flag in w1, new sp in x0 (null = threw during ToBoolean).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_cond(a: &mut asm::Asm, mode: u32, l_unwind: usize) {
    a.mov(0, 19);
    a.movz(1, mode, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (H_COND * 8) as u32);
    a.blr(16);
    a.cbz(0, true, l_unwind);
    a.mov(20, 0);
}

/// Per-pc operand-stack depth simulation; returns the maximum depth, or `None` on an
/// inconsistency (which would mean an emitter bug — refuse to compile rather than corrupt).
fn stack_depths(chunk: &Chunk) -> Option<usize> {
    use crate::bytecode::Op;
    let ops = chunk.jit_ops();
    let mut depth: Vec<Option<usize>> = vec![None; ops.len() + 1];
    let mut work = vec![(0usize, 0usize)];
    let mut max = 0usize;
    while let Some((pc, d)) = work.pop() {
        if pc >= ops.len() {
            continue;
        }
        match depth[pc] {
            Some(prev) if prev == d => continue,
            Some(_) => return None,
            None => depth[pc] = Some(d),
        }
        max = max.max(d);
        let (pops, pushes) = chunk.jit_stack_effect(pc)?;
        if d < pops {
            return None;
        }
        let next = d - pops + pushes;
        max = max.max(next);
        match &ops[pc] {
            Op::Jump(t) => work.push((*t as usize, next)),
            Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t) => {
                work.push((*t as usize, next));
                work.push((pc + 1, next));
            }
            Op::Return | Op::ReturnUndef | Op::Throw => {}
            Op::PushHandler(t) => {
                // The catch entry runs with the exception pushed on the entry depth.
                work.push((*t as usize, d + 1));
                work.push((pc + 1, next));
            }
            _ => work.push((pc + 1, next)),
        }
    }
    Some(max + 1) // headroom: GetMethod-style ops peak one above their settle depth
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// Execute a JIT-compiled chunk: mirrors `bytecode::run` (activation env, pooled slot buffer),
/// with the operand stack in a pooled flat buffer sized by the static analysis.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub fn run(
    i: &mut Interp,
    chunk: &Rc<Chunk>,
    code: &JitCode,
    env: &Env,
    this_val: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    let env = chunk.jit_make_run_env(i, env, &this_val, args);
    let (mut slots, mut stack) = i.vm_pool.pop().unwrap_or_default();
    let (n_params, n_slots) = chunk.jit_frame();
    let seed = n_params.min(args.len());
    slots.extend_from_slice(&args[..seed]);
    slots.resize(n_slots, Value::Undefined);
    for &s in chunk.jit_var_force_resets() {
        slots[s as usize] = Value::Undefined;
    }
    stack.clear();
    stack.reserve(code.max_stack);

    let helpers: [usize; N_HELPERS] = [
        crate::bytecode::jit_exec as *const () as usize,
        crate::bytecode::jit_cond as *const () as usize,
        crate::bytecode::jit_return as *const () as usize,
        crate::bytecode::jit_push_handler as *const () as usize,
        crate::bytecode::jit_pop_handler as *const () as usize,
        crate::bytecode::jit_unwind as *const () as usize,
    ];
    let stack_base = stack.as_mut_ptr();
    let mut ctx = JitCtx {
        helpers: helpers.as_ptr(),
        stack_base,
        final_sp: stack_base,
        interp: i as *mut Interp,
        chunk: Rc::as_ptr(chunk),
        env,
        this_val,
        slots: slots.as_mut_ptr(),
        inline_ic_safe: &i.inline_ic_safe as *const std::cell::Cell<bool> as *const u8,
        n_slots,
        handlers: Vec::new(),
        code_base: code.mem,
        pc_offsets: code.pc_offsets.as_ptr(),
        error: None,
        ret: Value::Undefined,
    };
    let entry: extern "C" fn(*mut JitCtx) -> u64 = unsafe { std::mem::transmute(code.mem) };
    let ok = entry(&mut ctx);
    // Drop any operands left on the raw stack (a throw can leave temporaries).
    unsafe {
        let mut p = ctx.stack_base;
        while p < ctx.final_sp {
            std::ptr::drop_in_place(p);
            p = p.add(1);
        }
    }
    slots.clear();
    stack.clear();
    if i.vm_pool.len() < 64 {
        i.vm_pool.push((slots, stack));
    }
    if ok == 1 {
        Ok(std::mem::take(&mut ctx.ret))
    } else {
        Err(ctx
            .error
            .take()
            .unwrap_or_else(|| Abrupt::Throw(Value::Undefined)))
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
pub fn run(
    _i: &mut Interp,
    _chunk: &Rc<Chunk>,
    _code: &JitCode,
    _env: &Env,
    _this_val: Value,
    _args: &[Value],
) -> Result<Value, Abrupt> {
    unreachable!("jit code cannot exist on this platform")
}
