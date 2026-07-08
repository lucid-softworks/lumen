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
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use crate::bytecode::UpdKind;
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
    /// Whether any template reads `JitCtx::global_body` (free-name caches): frame setup skips
    /// the realm-global borrow otherwise.
    pub needs_global: bool,
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
    /// [40] `Rc::as_ptr` of the activation env — what the inline LoadName template compares
    /// against the per-site name cache (see `bytecode::NameIc`).
    pub env_raw: *const u8,
    /// [48] Points at `this_val` below (set after construction): the inline LoadThis template
    /// copies the 24-byte Value and bumps its refcount from machine code.
    pub this_raw: *const Value,
    /// [56] The current realm's global `Object` (through the Rc and RefCell): the LoadName
    /// templates' global-mode path validates the cached shape/slot against it.
    pub global_body: *const u8,
    // ---- Rust-only fields ----
    pub interp: *mut Interp,
    pub chunk: *const Chunk,
    /// The activation env the chunk runs under. A raw pointer: the handle it aliases outlives
    /// the run — `run` keeps its freshly-made env in a local across the call; `run_moved` points
    /// at a handle the caller keeps alive (a local clone, or the env inside the callee's
    /// `Callable::User`, pinned by the callee object on the caller's operand stack).
    pub env_ref: *const Env,
    pub this_val: Value,
    pub n_slots: usize,
    /// Active `try` regions: (catch pc, operand-stack depth to unwind to).
    pub handlers: Vec<(u32, usize)>,
    pub code_base: *const u8,
    pub pc_offsets: *const u32,
    pub error: Option<Abrupt>,
    pub ret: Value,
}

/// The helper function table the emitted code indexes (see `JitCtx::helpers`); built once per
/// `Interp` (`Interp::jit_helpers`) so calls don't re-materialize it.
pub(crate) fn helper_table() -> [usize; N_HELPERS] {
    [
        crate::bytecode::jit_exec as *const () as usize,
        crate::bytecode::jit_cond as *const () as usize,
        crate::bytecode::jit_return as *const () as usize,
        crate::bytecode::jit_push_handler as *const () as usize,
        crate::bytecode::jit_pop_handler as *const () as usize,
        crate::bytecode::jit_unwind as *const () as usize,
        crate::bytecode::jit_call as *const () as usize,
    ]
}

/// Helper table indices (multiplied by 8 in the emitted `ldr`).
pub const H_EXEC: usize = 0;
pub const H_COND: usize = 1;
pub const H_RETURN: usize = 2;
pub const H_PUSH_HANDLER: usize = 3;
pub const H_POP_HANDLER: usize = 4;
pub const H_UNWIND: usize = 5;
pub const H_CALL: usize = 6;
pub const N_HELPERS: usize = 7;

/// ARM64 condition codes used by the inline templates.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_EQ: u32 = 0;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_NE: u32 = 1;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_HS: u32 = 2;
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
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const C_VS: u32 = 6;

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
            debug_assert!(imm_bytes.is_multiple_of(8) && imm_bytes / 8 < 4096);
            self.emit(0xF940_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// str xt, [xn, #imm]
        pub fn str_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes.is_multiple_of(8) && imm_bytes / 8 < 4096);
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
            debug_assert!(imm_bytes.is_multiple_of(4) && imm_bytes / 4 < 4096);
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
            debug_assert!(imm_bytes.is_multiple_of(8) && imm_bytes / 8 < 4096);
            self.emit(0xFD40_0000 | ((imm_bytes / 8) << 10) | (rn << 5) | rt);
        }
        /// str dt, [xn, #imm] (scaled)
        pub fn str_d_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes.is_multiple_of(8) && imm_bytes / 8 < 4096);
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
        /// fcvtzu wd, dn (float → unsigned 32-bit, round toward zero, saturating)
        pub fn fcvtzu_w_d(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E79_0000 | (rn << 5) | rd);
        }
        /// ucvtf dd, wn (unsigned 32-bit → double, exact)
        pub fn ucvtf_d_w(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E63_0000 | (rn << 5) | rd);
        }
        /// fcvtzs xd, dn (float → signed 64-bit, round toward zero, saturating)
        pub fn fcvtzs_x_d(&mut self, rd: u32, rn: u32) {
            self.emit(0x9E78_0000 | (rn << 5) | rd);
        }
        /// fcvtzs wd, dn (float → signed 32-bit, round toward zero, saturating)
        pub fn fcvtzs_w_d(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E78_0000 | (rn << 5) | rd);
        }
        /// scvtf dd, xn (signed 64-bit → double, round to nearest)
        pub fn scvtf_d_x(&mut self, rd: u32, rn: u32) {
            self.emit(0x9E62_0000 | (rn << 5) | rd);
        }
        /// scvtf dd, wn (signed 32-bit → double, exact)
        pub fn scvtf_d_w(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E62_0000 | (rn << 5) | rd);
        }
        /// frintz dd, dn (round toward zero to integral)
        pub fn frintz(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E65_C000 | (rn << 5) | rd);
        }
        /// fmov dd, xn (bit move)
        pub fn fmov_d_x(&mut self, rd: u32, rn: u32) {
            self.emit(0x9E67_0000 | (rn << 5) | rd);
        }
        /// fneg dd, dn
        pub fn fneg(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E61_4000 | (rn << 5) | rd);
        }
        /// fmov dd, dn
        pub fn fmov_d_d(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E60_4000 | (rn << 5) | rd);
        }
        /// stp dt1, dt2, [sp, #imm] (SIMD&FP 64-bit, signed offset)
        pub fn stp_d_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0x6D00_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// ldp dt1, dt2, [sp, #imm]
        pub fn ldp_d_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0x6D40_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// and/orr/eor wd, wn, wm — op: 0=and, 1=orr, 2=eor
        pub fn logic_w(&mut self, op: u32, rd: u32, rn: u32, rm: u32) {
            let bits = match op {
                0 => 0x0A00_0000u32,
                1 => 0x2A00_0000,
                _ => 0x4A00_0000,
            };
            self.emit(bits | (rm << 16) | (rn << 5) | rd);
        }
        /// lslv/lsrv/asrv wd, wn, wm (shift amount = wm mod 32, matching JS) — op: 0=lsl, 1=lsr, 2=asr
        pub fn shift_w(&mut self, op: u32, rd: u32, rn: u32, rm: u32) {
            let bits = match op {
                0 => 0x1AC0_2000u32,
                1 => 0x1AC0_2400,
                _ => 0x1AC0_2800,
            };
            self.emit(bits | (rm << 16) | (rn << 5) | rd);
        }
        /// add xd, xn, xm, lsl #shift
        pub fn add_shifted(&mut self, rd: u32, rn: u32, rm: u32, shift: u32) {
            debug_assert!(shift < 64);
            self.emit(0x8B00_0000 | (rm << 16) | (shift << 10) | (rn << 5) | rd);
        }
        /// cmp xn, xm (SUBS xzr, xn, xm)
        pub fn cmp_reg_x(&mut self, rn: u32, rm: u32) {
            self.emit(0xEB00_001F | (rm << 16) | (rn << 5));
        }
        /// lsr xd, xn, #shift (UBFM xd, xn, #shift, #63)
        pub fn lsr_imm(&mut self, rd: u32, rn: u32, shift: u32) {
            debug_assert!(shift < 64);
            self.emit(0xD340_FC00 | (shift << 16) | (rn << 5) | rd);
        }
        /// mov wd, wm (ORR wd, wzr, wm — zero-extends into the x register)
        pub fn mov_w(&mut self, rd: u32, rm: u32) {
            self.emit(0x2A00_03E0 | (rm << 16) | rd);
        }
        /// cmn wn, #imm12 (ADDS wzr, wn, #imm — `cmn wn, #1` tests for 0xFFFF_FFFF)
        pub fn cmn_imm_w(&mut self, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x3100_001F | (imm << 10) | (rn << 5));
        }
        /// cmn xn, #imm12 (ADDS xzr, xn, #imm — `cmn xn, #1` sets V exactly for xn == i64::MAX)
        pub fn cmn_imm_x(&mut self, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0xB100_001F | (imm << 10) | (rn << 5));
        }
        /// fcmp dn, #0.0
        pub fn fcmp_zero(&mut self, rn: u32) {
            self.emit(0x1E60_2008 | (rn << 5));
        }
        /// sub xd, xn, xm
        pub fn sub_reg(&mut self, rd: u32, rn: u32, rm: u32) {
            self.emit(0xCB00_0000 | (rm << 16) | (rn << 5) | rd);
        }
        /// sxtw xd, wn (SBFM xd, xn, #0, #31)
        pub fn sxtw(&mut self, rd: u32, rn: u32) {
            self.emit(0x9340_7C00 | (rn << 5) | rd);
        }
        /// asr wd, wn, #shift (SBFM wd, wn, #shift, #31)
        pub fn asr_imm_w(&mut self, rd: u32, rn: u32, shift: u32) {
            debug_assert!(shift < 32);
            self.emit(0x1300_7C00 | (shift << 16) | (rn << 5) | rd);
        }
        /// lsr wd, wn, #shift (UBFM wd, wn, #shift, #31)
        pub fn lsr_imm_w(&mut self, rd: u32, rn: u32, shift: u32) {
            debug_assert!(shift < 32);
            self.emit(0x5300_7C00 | (shift << 16) | (rn << 5) | rd);
        }
        /// lsl wd, wn, #shift (UBFM wd, wn, #(32-shift)%32, #(31-shift))
        pub fn lsl_imm_w(&mut self, rd: u32, rn: u32, shift: u32) {
            debug_assert!(shift < 32);
            let immr = (32 - shift) & 31;
            let imms = 31 - shift;
            self.emit(0x5300_0000 | (immr << 16) | (imms << 10) | (rn << 5) | rd);
        }
        /// and/orr/eor wd, wn, #imm (logical immediate; `field` from [`logical_imm_w`]) —
        /// op: 0=and, 1=orr, 2=eor
        pub fn logic_imm_w(&mut self, op: u32, rd: u32, rn: u32, field: u32) {
            let bits = match op {
                0 => 0x1200_0000u32,
                1 => 0x3200_0000,
                _ => 0x5200_0000,
            };
            self.emit(bits | (field << 10) | (rn << 5) | rd);
        }
        /// adds wd, wn, #imm12 (sets flags; V on i32 overflow)
        pub fn adds_imm_w(&mut self, rd: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x3100_0000 | (imm << 10) | (rn << 5) | rd);
        }
        /// subs wd, wn, #imm12 (sets flags; V on i32 overflow)
        pub fn subs_imm_w(&mut self, rd: u32, rn: u32, imm: u32) {
            debug_assert!(imm < 4096);
            self.emit(0x7100_0000 | (imm << 10) | (rn << 5) | rd);
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

    /// Encode a 32-bit logical immediate for AND/ORR/EOR (immediate form): the 12-bit
    /// `immr:imms` field to OR into the instruction at bit 10 (N is always 0 for the 32-bit
    /// variant). `None` when `v` is not a repeating rotated ones-run (0 and !0 included).
    pub fn logical_imm_w(v: u32) -> Option<u32> {
            if v == 0 || v == u32::MAX {
                return None;
            }
            // Smallest power-of-two period.
            let mut p = 32u32;
            while p > 2 {
                let h = p / 2;
                let mask = (1u64 << h) - 1;
                let mut periodic = true;
                let mut i = h;
                while i < 32 {
                    if (v as u64 >> i) & mask != v as u64 & mask {
                        periodic = false;
                        break;
                    }
                    i += h;
                }
                if !periodic {
                    break;
                }
                p = h;
            }
            let emask = if p == 32 { u32::MAX } else { (1u32 << p) - 1 };
            let elem = v & emask;
            let len = elem.count_ones();
            if len == 0 || len == p {
                return None;
            }
            let ones = ((1u64 << len) - 1) as u32;
            // The element must be ones(len) rotated right by immr (within p bits).
            for r in 0..p {
                let ror = if r == 0 {
                    ones
                } else {
                    ((ones >> r) | (ones << (p - r))) & emask
                };
                if ror == elem {
                    let imms = match p {
                        32 => 0x00,
                        16 => 0x20,
                        8 => 0x30,
                        4 => 0x38,
                        _ => 0x3C,
                    } | (len - 1);
                    return Some((r << 6) | imms);
                }
            }
            None
        }

    #[cfg(test)]
    mod tests {
        /// Brute-force decoder for the 32-bit logical-immediate field (N=0).
        fn decode(field: u32) -> Option<u32> {
            let immr = (field >> 6) & 0x3F;
            let imms = field & 0x3F;
            // Element size from the leading-ones pattern of imms.
            let (p, len) = match imms {
                s if s & 0x20 == 0 => (32u32, (s & 0x1F) + 1),
                s if s & 0x30 == 0x20 => (16, (s & 0x0F) + 1),
                s if s & 0x38 == 0x30 => (8, (s & 0x07) + 1),
                s if s & 0x3C == 0x38 => (4, (s & 0x03) + 1),
                s if s & 0x3E == 0x3C => (2, (s & 0x01) + 1),
                _ => return None,
            };
            if len >= p || immr >= p {
                return None;
            }
            let ones = ((1u64 << len) - 1) as u32;
            let emask = if p == 32 { u32::MAX } else { (1u32 << p) - 1 };
            let elem = if immr == 0 {
                ones
            } else {
                ((ones >> immr) | (ones << (p - immr))) & emask
            };
            let mut v = 0u32;
            let mut i = 0;
            while i < 32 {
                v |= elem << i;
                i += p;
            }
            Some(v)
        }

        #[test]
        fn logical_imm_w_roundtrip() {
            // Every encodable field decodes back to a value that re-encodes to itself.
            let mut seen = std::collections::HashMap::new();
            for field in 0u32..(1 << 12) {
                if let Some(v) = decode(field) {
                    seen.entry(v).or_insert(field);
                }
            }
            for (&v, _) in &seen {
                let enc = super::logical_imm_w(v).unwrap_or_else(|| {
                    panic!("0x{v:08x} should be encodable");
                });
                assert_eq!(decode(enc), Some(v), "0x{v:08x} enc {enc:03x}");
            }
            // Common masks used by the emitter.
            for m in [0x3fffu32, 0xfffffff, 0x7fff, 0xff, 1, 0x3fffffff] {
                assert!(super::logical_imm_w(m).is_some(), "0x{m:x}");
            }
            // Non-encodable values.
            for m in [0u32, u32::MAX, 0x12345678, 5] {
                if let Some(enc) = super::logical_imm_w(m) {
                    assert_eq!(decode(enc), Some(m));
                }
            }
            assert!(super::logical_imm_w(0).is_none());
            assert!(super::logical_imm_w(u32::MAX).is_none());
            assert!(super::logical_imm_w(0x12345678).is_none());
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
    // Debug: `LUMEN_JIT_DUMP=<substr>` prints the op stream of chunks whose leading slot names
    // contain the substring (empty value = all chunks) as they compile.
    if let Ok(pat) = std::env::var("LUMEN_JIT_DUMP") {
        let head: Vec<&str> = chunk.jit_slot_names().iter().take(4).map(|s| &**s).collect();
        let name = head.join(",");
        if pat.is_empty() || name.contains(&pat) {
            eprintln!("[jit-dump] fn({name}) {} ops", ops.len());
            for (pc, op) in ops.iter().enumerate() {
                eprintln!("[jit-dump]   {pc:>4}  {op:?}");
            }
        }
    }
    let fast: u32 = std::env::var("LUMEN_JIT_FAST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);
    // Whether the probed layout supports inline refcount bumps/decs (clone/drop of Str/Sym/Obj
    // without a helper call). All strong-count templates gate on this.
    let rc_ok = layout.valid && layout.rc_strong_off < 256;
    let rc_strong = layout.rc_strong_off as i32;
    let mut a = asm::Asm::new();
    // One label per bytecode pc (branch/catch targets bind as we emit).
    let pc_labels: Vec<usize> = (0..ops.len()).map(|_| a.new_label()).collect();
    let l_unwind = a.new_label();
    let l_ret_ok = a.new_label();
    let l_ret_throw = a.new_label();

    // ---- prologue ----
    // Frame: save fp/lr + x19..x22 (x19=ctx, x20=sp, x21=helpers, x22=slots) + d8..d15
    // (the numeric-chain emitter keeps its virtual operand stack in them).
    a.stp_pre(29, 30, -112);
    a.stp_off(19, 20, 16);
    a.stp_off(21, 22, 32);
    a.stp_d_off(8, 9, 48);
    a.stp_d_off(10, 11, 64);
    a.stp_d_off(12, 13, 80);
    a.stp_d_off(14, 15, 96);
    a.mov(19, 0); // ctx
    a.ldr_imm(21, 19, 0); // helpers table
    a.ldr_imm(20, 19, 8); // sp = stack_base
    a.ldr_imm(22, 19, 24); // local slots base

    // Branch/catch targets: a fused compare+branch may only swallow a following JumpIfFalse if
    // nothing can land on the branch op itself.
    let mut targeted = vec![false; ops.len() + 1];
    for op in ops {
        match op {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t)
            | Op::PushHandler(t) => targeted[*t as usize] = true,
            _ => {}
        }
    }

    // ---- op templates ----
    let mut pc_insn: Vec<u32> = Vec::with_capacity(ops.len());
    let mut skip = 0usize;
    for (pc, op) in ops.iter().enumerate() {
        a.bind(pc_labels[pc]);
        pc_insn.push(a.here() as u32);
        if skip > 0 {
            // Consumed by a fusion (chain / compare+branch / key-producer pair). The label and
            // pc-offset still bind here (harmless: nothing jumps into a fused region — checked).
            skip -= 1;
            continue;
        }
        // Loop-spanning chain: a fully-chainable, branch-free loop headed here runs with its
        // locals register-resident across the back edge. The plain templates for the region are
        // still emitted below (starting at `plain_h`) as the bail target; the head's canonical
        // label points at the chain entry, so plain back-edge jumps re-enter the chain.
        if fast & 32768 != 0 && rc_ok && targeted[pc] {
            if let Some(plan) = plan_loop(chunk, ops, pc, &targeted, layout, fast) {
                let plain_h = emit_loop_chain(&mut a, layout, &plan, &pc_labels);
                a.bind(plain_h);
                // Bails jump to interior pc labels, so the plain region below must never fuse
                // across them: mark every interior pc targeted (all fusions respect that).
                for p in pc + 1..=plan.jump_pc {
                    targeted[p] = true;
                }
                // Fall through: the plain template for this op (and the rest of the region)
                // emits as usual.
            }
        }
        // Numeric register chain: a run of ops whose values stay in FP registers end to end.
        if fast & 16384 != 0 && rc_ok {
            if let Some((chain, consumed)) = build_chain(chunk, ops, pc, &targeted, layout, fast)
            {
                emit_chain(&mut a, layout, &chain, &pc_labels, l_unwind);
                skip = consumed - 1;
                continue;
            }
        }
        // Fused equality + JumpIfFalse: the full inline equality template drives the branch
        // directly — numbers, nullish, identity, Bool payloads, string length — no intermediate
        // bool. (The ordered relations below keep their number-only fusion: any other operand
        // type coerces, which is the helper's job.)
        if fast & 2 != 0 && eq_inlinable(layout) {
            if let (
                Op::StrictEq | Op::StrictNotEq | Op::EqEq | Op::NotEq,
                Some(Op::JumpIfFalse(t)),
            ) = (op, ops.get(pc + 1))
            {
                if !targeted[pc + 1] {
                    emit_eq_inline(
                        &mut a,
                        layout,
                        pc as u32,
                        l_unwind,
                        matches!(op, Op::StrictEq | Op::StrictNotEq),
                        matches!(op, Op::NotEq | Op::StrictNotEq),
                        Some(pc_labels[*t as usize]),
                    );
                    skip = 1;
                    continue;
                }
            }
        }
        // Fused number-compare + JumpIfFalse: fcmp and branch directly on the negated condition
        // (IEEE unordered must jump for the ordered relations and for ==; must fall through for
        // !=) — the intermediate bool never materializes. Types other than two numbers take the
        // unfused pair via the helpers.
        if fast & 2 != 0 {
            if let (
                Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::StrictEq | Op::StrictNotEq | Op::EqEq
                | Op::NotEq,
                Some(Op::JumpIfFalse(t)),
            ) = (op, ops.get(pc + 1))
            {
                if !targeted[pc + 1] {
                    let neg = match op {
                        Op::Lt => 5,  // PL: !(a<b), true for unordered (NaN must jump)
                        Op::Gt => 13, // LE: !(a>b), true for unordered
                        Op::Le => 8,  // HI: !(a<=b), true for unordered
                        Op::Ge => 11, // LT: !(a>=b), true for unordered
                        Op::StrictEq | Op::EqEq => 1, // NE: !(a==b), true for unordered
                        _ => 0,       // EQ: !(a!=b); unordered IS "!=" → correctly no jump
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
                    a.sub_imm(20, 20, 48); // pop both operands (no bool pushed)
                    a.fcmp(0, 1);
                    a.b_cond(neg, pc_labels[*t as usize]);
                    a.b(done);
                    a.bind(slow);
                    // Unfused fallback: generic compare (pushes a bool), then pop-and-branch.
                    emit_exec(&mut a, pc as u32, l_unwind);
                    emit_cond(&mut a, COND_POP_TRUTHY, l_unwind);
                    a.cbz(1, false, pc_labels[*t as usize]);
                    a.bind(done);
                    skip = 1;
                    continue;
                }
            }
        }
        // Fused key-producer + element read: `x0[cur]` (LoadLocal;GetElemLocal) and `x[++cur]`
        // (UpdateLocal-pre;GetElemLocal) skip the key's stack round-trip entirely. All guards run
        // before any state is written (the pre-increment commits with the element copy), so the
        // slow path can re-run both ops through the helper cleanly.
        if fast & 1024 != 0 && elem_inlinable(layout) && !targeted[pc + 1] {
            let in_range = |s: u16| (s as u32) * 24 + 16 < 4096;
            let pair = match (op, ops.get(pc + 1)) {
                (Op::LoadLocal(k), Some(Op::GetElemLocal(x))) if in_range(*k) && in_range(*x) => {
                    Some((*x as u32 * 24, KeySrc::Slot(*k as u32 * 24)))
                }
                (
                    Op::UpdateLocal(k, kind @ (UpdKind::PreInc | UpdKind::PreDec)),
                    Some(Op::GetElemLocal(x)),
                ) if in_range(*k) && in_range(*x) => Some((
                    *x as u32 * 24,
                    KeySrc::SlotPre(*k as u32 * 24, matches!(kind, UpdKind::PreDec)),
                )),
                _ => None,
            };
            if let Some((x_off, key)) = pair {
                emit_elem_local_keyed(
                    &mut a,
                    layout,
                    x_off,
                    &[pc as u32, pc as u32 + 1],
                    l_unwind,
                    ElemLocalKind::Get,
                    key,
                );
                skip = 1;
                continue;
            }
        }
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
            // ---- inline property cache: shape-validated read (`this.x`, proto constants) ----
            Op::GetProp(n, cache) if fast & 256 != 0 && get_method_inlinable(layout) => {
                let arr_ok = !chunk
                    .jit_name(*n)
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit());
                emit_prop_load_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                    false,
                    arr_ok,
                );
            }
            Op::ToPropKey | Op::ToPropKeyLocal(_) if fast & 64 != 0 => {
                // A Num or Str key passes through untouched (the overwhelmingly common case);
                // anything else — real coercion plus the nullish-base check — takes the helper.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_EQ, done);
                a.cmp_imm_w(9, 6);
                a.b_cond(C_EQ, done);
                a.b(slow);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Dup if fast & 64 != 0 && rc_ok => {
                // Copy the top value; refcounted payloads bump inline, BigInt takes the helper.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 5);
                a.b_cond(C_EQ, slow);
                a.ldur(10, 20, -24);
                a.ldur(11, 20, -16);
                a.ldur(12, 20, -8);
                a.stur(10, 20, 0);
                a.stur(11, 20, 8);
                a.stur(12, 20, 16);
                let nobump = a.new_label();
                a.cmp_imm_w(9, 6);
                a.b_cond(C_LO, nobump);
                a.ldur(13, 11, rc_strong);
                a.add_imm(13, 13, 1);
                a.stur(13, 11, rc_strong);
                a.bind(nobump);
                a.add_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::LoadThis if fast & 32768 != 0 && rc_ok => {
                // Copy ctx.this_val (24 bytes) and bump its refcount inline; only a BigInt
                // `this` (impossible in practice, but be safe) takes the helper.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldr_imm(9, 19, 48); // ctx.this_raw
                a.ldrb_imm(10, 9, 0);
                a.cmp_imm_w(10, 5);
                a.b_cond(C_EQ, slow);
                a.ldr_imm(11, 9, 0);
                a.ldr_imm(12, 9, 8);
                a.ldr_imm(13, 9, 16);
                a.stur(11, 20, 0);
                a.stur(12, 20, 8);
                a.stur(13, 20, 16);
                let nobump = a.new_label();
                a.cmp_imm_w(10, 6);
                a.b_cond(C_LO, nobump);
                a.ldur(14, 12, rc_strong);
                a.add_imm(14, 14, 1);
                a.stur(14, 12, rc_strong);
                a.bind(nobump);
                a.add_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            // ---- inline free-name cache (`width` in a hot loop body) ----
            Op::LoadName(_, cache) if fast & 8192 != 0 && load_name_inlinable(layout) => {
                emit_load_name_inline(
                    &mut a,
                    layout,
                    chunk.jit_name_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                    false,
                );
            }
            Op::LoadNameForCall(_, cache) if fast & 8192 != 0 && load_name_inlinable(layout) => {
                emit_load_name_inline(
                    &mut a,
                    layout,
                    chunk.jit_name_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                    true,
                );
            }
            // ---- inline dense-element fast paths (`a[i]` on plain objects/arrays) ----
            Op::GetElem if fast & 1024 != 0 && elem_inlinable(layout) => {
                emit_get_elem_inline(&mut a, layout, pc as u32, l_unwind);
            }
            Op::SetElemDrop if fast & 2048 != 0 && elem_inlinable(layout) => {
                emit_set_elem_inline(&mut a, layout, pc as u32, l_unwind, false);
            }
            Op::SetElem if fast & 4096 != 0 && elem_inlinable(layout) => {
                emit_set_elem_inline(&mut a, layout, pc as u32, l_unwind, true);
            }
            // ---- fused parameter-slot element ops (no receiver stack traffic or refcounting) ----
            Op::GetElemLocal(slot)
                if fast & 1024 != 0
                    && elem_inlinable(layout)
                    && (*slot as u32) * 24 + 16 < 4096 =>
            {
                emit_elem_local_inline(&mut a, layout, *slot as u32 * 24, pc as u32, l_unwind, ElemLocalKind::Get);
            }
            Op::SetElemLocalDrop(slot)
                if fast & 2048 != 0
                    && elem_inlinable(layout)
                    && (*slot as u32) * 24 + 16 < 4096 =>
            {
                emit_elem_local_inline(&mut a, layout, *slot as u32 * 24, pc as u32, l_unwind, ElemLocalKind::SetDrop);
            }
            Op::SetElemLocal(slot)
                if fast & 4096 != 0
                    && elem_inlinable(layout)
                    && (*slot as u32) * 24 + 16 < 4096 =>
            {
                emit_elem_local_inline(&mut a, layout, *slot as u32 * 24, pc as u32, l_unwind, ElemLocalKind::SetKeep);
            }
            // ---- inline property cache: method load (`obj.m(...)`) ----
            Op::GetMethod(n, cache) if fast & 512 != 0 && get_method_inlinable(layout) => {
                let arr_ok = !chunk
                    .jit_name(*n)
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit());
                emit_prop_load_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                    true,
                    arr_ok,
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
            // Int32 ops on two numbers: ToInt32 = truncate + wrap to 32 bits. fcvtzs to x
            // truncates; taking the low 32 bits is the mod-2^32 wrap. The scvtf/frintz
            // round-trip proves no i64 saturation happened (NaN/±Inf/|x|≥2^63 all fail it and
            // take the helper, which applies the spec's zero/wrap semantics).
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
                if fast & 1 != 0 =>
            {
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -48);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -24);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -40); // lhs
                a.ldur_d(1, 20, -16); // rhs
                a.fcvtzs_x_d(9, 0);
                a.scvtf_d_x(2, 9);
                a.frintz(3, 0);
                a.fcmp(2, 3);
                a.b_cond(C_NE, slow);
                // x == +2^63 exactly saturates yet passes the round-trip (2^63-1 re-rounds to
                // 2^63): cmn #1 sets V only for i64::MAX — send it to the helper.
                a.cmn_imm_x(9, 1);
                a.b_cond(6, slow); // VS
                a.fcvtzs_x_d(10, 1);
                a.scvtf_d_x(2, 10);
                a.frintz(3, 1);
                a.fcmp(2, 3);
                a.b_cond(C_NE, slow);
                a.cmn_imm_x(10, 1);
                a.b_cond(6, slow); // VS
                match op {
                    Op::BitAnd => a.logic_w(0, 11, 9, 10),
                    Op::BitOr => a.logic_w(1, 11, 9, 10),
                    Op::BitXor => a.logic_w(2, 11, 9, 10),
                    Op::Shl => a.shift_w(0, 11, 9, 10),
                    Op::UShr => a.shift_w(1, 11, 9, 10),
                    _ => a.shift_w(2, 11, 9, 10), // Shr
                }
                if matches!(op, Op::UShr) {
                    a.ucvtf_d_w(0, 11); // >>> yields an unsigned 32-bit result
                } else {
                    a.scvtf_d_w(0, 11);
                }
                a.stur_d(0, 20, -40);
                a.sub_imm(20, 20, 24);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::StrictEq | Op::StrictNotEq | Op::EqEq | Op::NotEq
                if fast & 2 != 0 && eq_inlinable(layout) =>
            {
                emit_eq_inline(
                    &mut a,
                    layout,
                    pc as u32,
                    l_unwind,
                    matches!(op, Op::StrictEq | Op::StrictNotEq),
                    matches!(op, Op::NotEq | Op::StrictNotEq),
                    None,
                );
            }
            Op::Not if fast & 131072 != 0 && eq_inlinable(layout) => {
                emit_not_inline(&mut a, layout, pc as u32, l_unwind);
            }
            Op::SetPropDrop(_, cache) if fast & 65536 != 0 && rc_ok && set_prop_inlinable(layout) => {
                emit_set_prop_inline(&mut a, layout, chunk.jit_cache_ptr(*cache), pc as u32, l_unwind);
            }
            Op::UpdateProp(_, cache, kind)
                if fast & 65536 != 0 && rc_ok && set_prop_inlinable(layout) =>
            {
                emit_update_prop_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    *kind,
                    pc as u32,
                    l_unwind,
                );
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
                if rc_ok {
                    // Refcounted values (Str/Sym/Obj) clone inline: copy + strong++. Only a
                    // BigInt (compound Rc payload at a non-fixed offset) takes the helper.
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_EQ, slow);
                    a.ldr_imm(10, 22, off);
                    a.ldr_imm(11, 22, off + 8);
                    a.ldr_imm(12, 22, off + 16);
                    a.stur(10, 20, 0);
                    a.stur(11, 20, 8);
                    a.stur(12, 20, 16);
                    let nobump = a.new_label();
                    a.cmp_imm_w(9, 6);
                    a.b_cond(C_LO, nobump);
                    a.ldur(13, 11, rc_strong);
                    a.add_imm(13, 13, 1);
                    a.stur(13, 11, rc_strong);
                    a.bind(nobump);
                } else {
                    a.cmp_imm_w(9, 4); // refcounted → slow (must clone)
                    a.b_cond(C_HI, slow);
                    a.ldr_imm(10, 22, off);
                    a.ldr_imm(11, 22, off + 8);
                    a.stur(10, 20, 0);
                    a.stur(11, 20, 8);
                }
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
                if rc_ok {
                    // A refcounted old value drops inline when it isn't the last reference;
                    // BigInt and a to-be-freed value take the helper (real destructor).
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_EQ, slow);
                    let mv = a.new_label();
                    a.cmp_imm_w(9, 6);
                    a.b_cond(C_LO, mv);
                    a.ldr_imm(10, 22, off + 8);
                    a.ldur(9, 10, rc_strong);
                    a.cmp_imm_x(9, 1);
                    a.b_cond(C_LS, slow);
                    a.sub_imm(9, 9, 1);
                    a.stur(9, 10, rc_strong);
                    a.bind(mv);
                } else {
                    a.cmp_imm_w(9, 4); // old value refcounted → slow (must drop)
                    a.b_cond(C_HI, slow);
                }
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
                if rc_ok {
                    // A refcounted top drops inline (strong--) unless it is the last reference
                    // (real destructor) or a BigInt (compound payload) — those take the helper.
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_EQ, slow);
                    let plain = a.new_label();
                    a.cmp_imm_w(9, 6);
                    a.b_cond(C_LO, plain);
                    a.ldur(10, 20, -16);
                    a.ldur(9, 10, rc_strong);
                    a.cmp_imm_x(9, 1);
                    a.b_cond(C_LS, slow);
                    a.sub_imm(9, 9, 1);
                    a.stur(9, 10, rc_strong);
                    a.bind(plain);
                } else {
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_HI, slow); // refcounted → slow (must drop)
                }
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
            // Calls take the dedicated helper: same contract as the generic one, minus the full
            // op dispatch (they dominate helper traffic in call-heavy code).
            Op::Call(..) | Op::CallWithThis(..) => {
                a.mov(0, 19);
                a.movz(1, pc as u32, 0);
                a.mov(2, 20);
                a.ldr_imm(16, 21, (H_CALL * 8) as u32);
                a.blr(16);
                a.mov(20, 0);
                a.cbnz(1, false, l_unwind);
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
    a.ldp_d_off(8, 9, 48);
    a.ldp_d_off(10, 11, 64);
    a.ldp_d_off(12, 13, 80);
    a.ldp_d_off(14, 15, 96);
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    a.ldp_post(29, 30, 112);
    a.ret();
    a.bind(l_ret_throw);
    a.str_imm(20, 19, 16);
    a.movz(0, 0, 0);
    a.ldp_d_off(8, 9, 48);
    a.ldp_d_off(10, 11, 64);
    a.ldp_d_off(12, 13, 80);
    a.ldp_d_off(14, 15, 96);
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    a.ldp_post(29, 30, 112);
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
            needs_global: ops
                .iter()
                .any(|o| matches!(o, Op::LoadName(..) | Op::LoadNameForCall(..))),
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
    let en = layout.obj_props + layout.props_entries + layout.vec_ptr_off;
    let enl = layout.obj_props + layout.props_entries + layout.vec_len_off;
    layout.valid
        && layout.obj_from_rc < 4096
        && layout.obj_exotic < 4096
        && layout.obj_ic_plain < 4096
        && sh.is_multiple_of(4)
        && sh / 4 < 4096
        && en.is_multiple_of(8)
        && en / 8 < 4096
        && enl.is_multiple_of(8)
        && enl / 8 < 4096
        && layout.entry_accessor < 4096
        && layout.entry_value + 16 < 256
        && layout.rc_strong_off < 256
        && layout.entry_size < 0x1_0000
}

/// Inline shape-validated property load, unified over `GetProp` (`method == false`: pop the
/// receiver, push the value in its slot) and `GetMethod` (`method == true`: the receiver stays —
/// it is re-used as `this` — and the method pushes above it), and over IC depths 0..=2:
/// the value may live on the receiver itself, its prototype, or two hops up (a subclass
/// hierarchy). Every hop re-follows the live proto pointer and re-validates exotic-None +
/// `ic_plain` + shape — a shape match on a non-holder hop proves it still lacks the name (see
/// [`crate::bytecode::IcState`]); depth 2 additionally requires the recorded `mid_shape`
/// (`mid_ok`). Every guard branches to `slow` before any state is written, so the fallback
/// re-runs the op cleanly. A BigInt value (compound payload), an accessor, any guard miss, or a
/// last-reference receiver (whose pop-drop would free) falls to the checked helper.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_prop_load_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
    method: bool,
    // Whether an `Exotic::Array` receiver may shape-validate: true when the site's (compile-time)
    // name cannot be an element key — element inserts don't transition an array's shape, but
    // element keys are all canonical indices, so a name that doesn't start with a digit cannot
    // collide with one. Prototype hops stay `Exotic::None`-only.
    arr_ok: bool,
) {
    use crate::bytecode::{
        IC_OFF_DEPTH, IC_OFF_HOLDER_SHAPE, IC_OFF_MID_OK, IC_OFF_MID_SHAPE, IC_OFF_RECV_SHAPE,
        IC_OFF_SLOT,
    };
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let pr = layout.obj_proto as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let en_len = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    let d1 = a.new_label();
    let load = a.new_label();
    // 1. receiver must be an Obj (tag 8); x10 = its stored Rc pointer, kept live for the final
    //    receiver drop (GetProp) — hop walking uses x17.
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -16);
    if !method {
        // receiver refcount > 1 (so the pop-drop below never frees)
        a.ldur(9, 10, strong);
        a.cmp_imm_x(9, 1);
        a.b_cond(C_LS, slow);
    }
    // 2. cache: depth + slot
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.ldr_w_imm(13, 12, IC_OFF_SLOT);
    // 3. receiver hop: exotic None (or Array when `arr_ok` — but only as a NON-holder, so an
    //    Array receiver additionally requires depth ≥ 1: its shape proves named-key ABSENCE, not
    //    slot positions, because element entries occupy slots without transitioning the shape),
    //    plain, shape == recv_shape; x11 = receiver object base
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(14, 11, ex);
    if arr_ok {
        let ex_ok = a.new_label();
        a.cmp_imm_w(14, none_tag);
        a.b_cond(C_EQ, ex_ok);
        a.cmp_imm_w(14, layout.exotic_array_tag as u32);
        a.b_cond(C_NE, slow);
        a.cbz(9, false, slow); // Array receiver must not be the holder (w9 = depth)
        a.bind(ex_ok);
    } else {
        a.cmp_imm_w(14, none_tag);
        a.b_cond(C_NE, slow);
    }
    a.ldrb_imm(14, 11, plain);
    a.cbz(14, false, slow);
    a.ldr_w_imm(14, 11, sh);
    a.ldr_w_imm(16, 12, IC_OFF_RECV_SHAPE);
    a.cmp_reg_w(14, 16);
    a.b_cond(C_NE, slow);
    // 4. depth routing: 0 → holder is the receiver; 1 → one hop; 2 → mid hop then fall into d1.
    a.cbz(9, false, load);
    a.cmp_imm_w(9, 1);
    a.b_cond(C_EQ, d1);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(14, 12, IC_OFF_MID_OK);
    a.cbz(14, false, slow);
    // depth-2 mid hop: follow the live proto, validate against mid_shape
    a.ldr_imm(17, 11, pr); // Option<Gc> niche: pointer or 0
    a.cbz(17, true, slow);
    a.add_imm(11, 17, rcv);
    a.ldrb_imm(14, 11, ex);
    a.cmp_imm_w(14, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(14, 11, plain);
    a.cbz(14, false, slow);
    a.ldr_w_imm(14, 11, sh);
    a.ldr_w_imm(16, 12, IC_OFF_MID_SHAPE);
    a.cmp_reg_w(14, 16);
    a.b_cond(C_NE, slow);
    // 5. holder hop (depth 1 entry point; depth 2 falls through): validate holder_shape
    a.bind(d1);
    a.ldr_imm(17, 11, pr);
    a.cbz(17, true, slow);
    a.add_imm(11, 17, rcv);
    a.ldrb_imm(14, 11, ex);
    a.cmp_imm_w(14, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(14, 11, plain);
    a.cbz(14, false, slow);
    a.ldr_w_imm(14, 11, sh);
    a.ldr_w_imm(16, 12, IC_OFF_HOLDER_SHAPE);
    a.cmp_reg_w(14, 16);
    a.b_cond(C_NE, slow);
    // 6. x11 = holder base: bounds-check the cached slot against the live entries length
    //    (defense in depth — fills only record exact-slot holders, but an OOB read through a
    //    stale cache would be memory-unsafe, so verify), then entry = entries + slot*size;
    //    data property; non-BigInt
    a.bind(load);
    a.ldr_imm(16, 11, en_len);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    a.ldurb(9, 15, ev); // w9 = value tag (kept live through the loads below)
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    // --- commit: everything validated; from here only writes ---
    a.ldur(12, 15, ev);
    a.ldur(13, 15, ev + 8); // payload word (the Rc pointer for tags 6..8)
    a.ldur(14, 15, ev + 16);
    // clone: a refcounted value (tag ≥ 6) needs its strong count bumped
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 13, strong);
    a.add_imm(16, 16, 1);
    a.stur(16, 13, strong);
    a.bind(nobump);
    if method {
        // receiver stays at [-24]; push the method above it
        a.stur(12, 20, 0);
        a.stur(13, 20, 8);
        a.stur(14, 20, 16);
        a.add_imm(20, 20, 24);
    } else {
        // drop the receiver (strong was > 1: decrement, no free). If the value IS the receiver
        // the bump above already balanced this (the count is re-read).
        a.ldur(9, 10, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 10, strong);
        // overwrite the receiver slot with the value (pop obj + push value = same depth)
        a.stur(12, 20, -24);
        a.stur(13, 20, -16);
        a.stur(14, 20, -8);
    }
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


/// Same gate as [`get_prop_inlinable`] plus the `writable` byte (the store re-checks it — an
/// in-place defineProperty can flip attributes without changing the shape).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn set_prop_inlinable(layout: &crate::value::JitLayout) -> bool {
    get_prop_inlinable(layout) && layout.entry_writable < 4096
}

/// Inline `this.x++` / `--` (`UpdateProp`): the read and the write both target the cached own
/// data slot — exactly what a depth-0 IC hit on the VM path does (`get_prop_ic` then
/// `set_prop_ic`) — so a shape-validated receiver whose slot holds a Num updates in place with
/// one FP add. Anything else (accessor, non-writable, non-Num old value, shape/depth miss,
/// exotic receiver, last-reference receiver) falls to the checked helper before any state is
/// written.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_update_prop_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    kind: UpdKind,
    pc: u32,
    l_unwind: usize,
) {
    use crate::bytecode::{IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT};
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. stack: [obj @ -24] — receiver must be an Obj with refcount > 1
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -16);
    a.ldur(9, 10, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, slow);
    // 2. cache: depth 0, slot + shape
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.cbnz(9, false, slow);
    a.ldr_w_imm(13, 12, IC_OFF_SLOT);
    a.ldr_w_imm(14, 12, IC_OFF_RECV_SHAPE);
    // 3. ordinary receiver, shape match
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(9, 11, plain);
    a.cbz(9, false, slow);
    a.ldr_w_imm(9, 11, sh);
    a.cmp_reg_w(9, 14);
    a.b_cond(C_NE, slow);
    // 4. bounds-check the cached slot, then entry: data property, writable, holding a Num
    a.ldr_imm(16, 11, (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    a.ldrb_imm(9, 15, ew);
    a.cbz(9, false, slow);
    a.ldurb(9, 15, ev);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    // --- commit: d0 = old, d2 = old ± 1, written in place ---
    a.ldur_d(0, 15, ev + 8);
    a.fmov_one(1);
    let dec = matches!(kind, UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard);
    a.f_arith(if dec { 1 } else { 0 }, 2, 0, 1);
    a.stur_d(2, 15, ev + 8);
    // drop the receiver (strong was > 1)
    a.ldur(9, 10, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, strong);
    // result per kind: Pre* push the new value, Post* the old, *Discard nothing.
    match kind {
        UpdKind::PreInc | UpdKind::PreDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, -24);
            a.stur_d(2, 20, -16);
        }
        UpdKind::PostInc | UpdKind::PostDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, -24);
            a.stur_d(0, 20, -16);
        }
        UpdKind::IncDiscard | UpdKind::DecDiscard => {
            a.sub_imm(20, 20, 24);
        }
    }
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Gate for the inline equality / Not templates: the Obj arms read the receiver's `ic_plain`
/// byte, so those offsets must fit their instructions' immediate ranges.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn eq_inlinable(layout: &crate::value::JitLayout) -> bool {
    layout.valid
        && layout.rc_strong_off < 256
        && layout.obj_from_rc < 4096
        && layout.obj_ic_plain < 4096
        && crate::lstr::LEN_OFF.is_multiple_of(4)
        && crate::lstr::LEN_OFF / 4 < 4096
}

/// Inline own-property store (`this.x = v`, statement position → `SetPropDrop`): the machine-code
/// mirror of `Interp::try_ic_set`'s shape fast path. Validates the receiver by shape (a match
/// proves the cached slot still maps this name), re-checks `accessor`/`writable`, then *moves*
/// the 24-byte value off the operand stack into the slot — a pure value overwrite never changes
/// the shape, so no cache invalidation is needed. The old value drops inline (strong-- when
/// refcounted and not the last reference); a BigInt old value (compound drop), a last-reference
/// old value or receiver, an accessor/non-writable slot, a shape or depth miss, and any exotic
/// receiver all fall to the checked helper. Every guard branches to `slow` before any state is
/// written, so the fallback re-runs the op cleanly.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_set_prop_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
) {
    use crate::bytecode::{IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT};
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. stack: [obj @ -48, v @ -24] — receiver must be an Obj (tag 8)
    a.ldurb(9, 20, -48);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -40); // receiver rc_ptr
    // 2. receiver refcount > 1 (so the pop-drop below never frees)
    a.ldur(9, 10, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, slow);
    // 3. cache: depth must be 0 (own writable data property wins OrdinarySet regardless of the
    //    prototype chain); load slot + cached receiver shape
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.cbnz(9, false, slow);
    a.ldr_w_imm(13, 12, IC_OFF_SLOT);
    a.ldr_w_imm(14, 12, IC_OFF_RECV_SHAPE);
    // 4. object base; exotic None, and not a side-table exotic (proxy/typed-array/namespace)
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(9, 11, plain);
    a.cbz(9, false, slow);
    // 5. shape id matches
    a.ldr_w_imm(9, 11, sh);
    a.cmp_reg_w(9, 14);
    a.b_cond(C_NE, slow);
    // 6. bounds-check the cached slot, then entry base = entries data ptr + slot*entry_size
    a.ldr_imm(16, 11, (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 7. data property, writable
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    a.ldrb_imm(9, 15, ew);
    a.cbz(9, false, slow);
    // 8. old value: trivially droppable (tag ≤ 4), or refcounted with strong > 1 (inline dec);
    //    BigInt or a last reference → helper. An old value that IS the receiver (`o.x === o`)
    //    also bails: its dec and the receiver dec below hit the same counter, and the two
    //    independent strong > 1 guards would let the pair scribble it to 0 without running the
    //    destructor. w9 = old tag, x12 = old payload, x14 = old strong.
    a.ldurb(9, 15, ev);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    let old_plain = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, old_plain);
    a.ldur(12, 15, ev + 8);
    a.cmp_reg_x(12, 10);
    a.b_cond(C_EQ, slow);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow);
    a.bind(old_plain);
    // --- commit: everything validated; from here only writes ---
    // move v into the entry (24 bytes; a refcounted payload moves, not clones)
    a.ldur(13, 20, -24);
    a.ldur(16, 20, -16);
    a.ldur(17, 20, -8);
    a.stur(13, 15, ev);
    a.stur(16, 15, ev + 8);
    a.stur(17, 15, ev + 16);
    // drop the old value (refcounted: strong was > 1, so this never frees)
    let no_old_dec = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, no_old_dec);
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.bind(no_old_dec);
    // drop the receiver (strong was > 1)
    a.ldur(9, 10, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, strong);
    // pop both operands, push nothing
    a.sub_imm(20, 20, 48);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Inline equality (`==` / `!=` / `===` / `!==`): every case the helper would resolve *without
/// coercion or content compares*, in machine code. Both-number pairs FCMP (IEEE: unordered is
/// unequal); loose nullish operands resolve by the other side's tag; same-tag Bools compare
/// payloads; same-tag Sym/Obj compare identity; same-tag Strs compare identity, then length (a
/// length mismatch is a definitive "not equal"; equal lengths fall to the helper's content
/// compare); strict different-tag pairs are unequal outright. Everything else — BigInt, coercing
/// mixed-type pairs, a refcounted operand that is a last reference (its drop runs a real
/// destructor), a loose nullish-vs-object compare on a non-ordinary object (`ic_plain` off —
/// which includes the `[[IsHTMLDDA]]` object) — takes the helper. Every guard branches to `slow`
/// before any state is written. With `branch`, the result drives a fused `JumpIfFalse` directly
/// (no Bool materializes); otherwise the Bool pushes in place of the operands.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_eq_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    pc: u32,
    l_unwind: usize,
    strict: bool,
    negate: bool,
    branch: Option<usize>,
) {
    let strong = layout.rc_strong_off as i32;
    let len_off = crate::lstr::LEN_OFF as u32;
    let slow = a.new_label();
    let done = a.new_label();
    let l_num = a.new_label();
    let l_sametag = a.new_label();
    let l_bool = a.new_label();
    let l_str = a.new_label();
    let l_ptr = a.new_label();
    let l_ptr_same = a.new_label();
    let l_true = a.new_label();
    let l_false = a.new_label();
    let l_have = a.new_label();
    // stack: [a @ -48, b @ -24]; w9 = tag_a, w10 = tag_b
    a.ldurb(9, 20, -48);
    a.ldurb(10, 20, -24);
    let l_notnum = a.new_label();
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, l_notnum);
    a.cmp_imm_w(10, 4);
    a.b_cond(C_EQ, l_num);
    a.bind(l_notnum);
    if !strict {
        // Loose nullish: undefined/null equal each other and nothing else (helper handles the
        // IsHTMLDDA exception via the inline_ic_safe gate below).
        let la_null = a.new_label();
        let lb_null = a.new_label();
        a.cmp_imm_w(9, 0);
        a.b_cond(C_EQ, la_null);
        a.cmp_imm_w(9, 2);
        a.b_cond(C_EQ, la_null);
        a.cmp_imm_w(10, 0);
        a.b_cond(C_EQ, lb_null);
        a.cmp_imm_w(10, 2);
        a.b_cond(C_EQ, lb_null);
        a.b(l_sametag);
        // a is nullish: equal iff b is nullish; otherwise false, dropping a refcounted b.
        a.bind(la_null);
        a.cmp_imm_w(10, 0);
        a.b_cond(C_EQ, l_true);
        a.cmp_imm_w(10, 2);
        a.b_cond(C_EQ, l_true);
        a.cmp_imm_w(10, 5);
        a.b_cond(C_EQ, slow); // BigInt → helper
        a.cmp_imm_w(10, 6);
        a.b_cond(C_LO, l_false); // Bool/Num: no drop needed
        a.ldur(13, 20, -16);
        let la_drop = a.new_label();
        a.cmp_imm_w(10, 8);
        a.b_cond(C_NE, la_drop);
        // nullish == Obj is only false for an ordinary object (ic_plain rules out IsHTMLDDA)
        a.add_imm(11, 13, layout.obj_from_rc as u32);
        a.ldrb_imm(11, 11, layout.obj_ic_plain as u32);
        a.cbz(11, false, slow);
        a.bind(la_drop);
        a.ldur(14, 13, strong);
        a.cmp_imm_x(14, 1);
        a.b_cond(C_LS, slow);
        a.sub_imm(14, 14, 1);
        a.stur(14, 13, strong);
        a.b(l_false);
        // b is nullish (a is not): false, dropping a refcounted a.
        a.bind(lb_null);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, l_false);
        a.ldur(12, 20, -40);
        let lb_drop = a.new_label();
        a.cmp_imm_w(9, 8);
        a.b_cond(C_NE, lb_drop);
        a.add_imm(11, 12, layout.obj_from_rc as u32);
        a.ldrb_imm(11, 11, layout.obj_ic_plain as u32);
        a.cbz(11, false, slow);
        a.bind(lb_drop);
        a.ldur(14, 12, strong);
        a.cmp_imm_x(14, 1);
        a.b_cond(C_LS, slow);
        a.sub_imm(14, 14, 1);
        a.stur(14, 12, strong);
        a.b(l_false);
    }
    a.bind(l_sametag);
    let l_diff = a.new_label();
    a.cmp_reg_w(9, 10);
    a.b_cond(C_NE, if strict { l_diff } else { slow });
    if strict {
        // Same-tag undefined/null are equal (loose routed them above).
        a.cmp_imm_w(9, 2);
        a.b_cond(C_LS, l_true);
    }
    a.cmp_imm_w(9, 3);
    a.b_cond(C_EQ, l_bool);
    a.cmp_imm_w(9, 6);
    a.b_cond(C_EQ, l_str);
    a.cmp_imm_w(9, 7);
    a.b_cond(C_HS, l_ptr); // Sym/Obj: identity
    a.b(slow); // BigInt
    a.bind(l_bool);
    a.ldurb(12, 20, -47);
    a.ldurb(13, 20, -23);
    a.cmp_reg_w(12, 13);
    a.cset_w(11, C_EQ);
    a.b(l_have);
    // Sym/Obj identity: same pointer → equal (dec by 2; both stack handles die), different →
    // unequal (dec each; both guarded > 1 first so neither dec frees).
    a.bind(l_ptr);
    a.ldur(12, 20, -40);
    a.ldur(13, 20, -16);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, l_ptr_same);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow);
    a.ldur(15, 13, strong);
    a.cmp_imm_x(15, 1);
    a.b_cond(C_LS, slow);
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.sub_imm(15, 15, 1);
    a.stur(15, 13, strong);
    a.b(l_false);
    a.bind(l_ptr_same);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 2);
    a.b_cond(C_LS, slow); // dec by 2 must not reach 0 (that drop runs a destructor)
    a.sub_imm(14, 14, 2);
    a.stur(14, 12, strong);
    a.b(l_true);
    // Str: identity → equal; different lengths → unequal; same length → helper (content).
    a.bind(l_str);
    a.ldur(12, 20, -40);
    a.ldur(13, 20, -16);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, l_ptr_same);
    a.ldr_w_imm(14, 12, len_off);
    a.ldr_w_imm(15, 13, len_off);
    a.cmp_reg_w(14, 15);
    a.b_cond(C_EQ, slow);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow);
    a.ldur(15, 13, strong);
    a.cmp_imm_x(15, 1);
    a.b_cond(C_LS, slow);
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.sub_imm(15, 15, 1);
    a.stur(15, 13, strong);
    a.b(l_false);
    if strict {
        // Different tags (both-number already peeled off): strictly unequal. Guard BOTH drops
        // before either dec so the slow fallback re-runs the op against untouched state.
        a.bind(l_diff);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.cmp_imm_w(10, 5);
        a.b_cond(C_EQ, slow);
        let ga = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, ga);
        a.ldur(12, 20, -40);
        a.ldur(14, 12, strong);
        a.cmp_imm_x(14, 1);
        a.b_cond(C_LS, slow);
        a.bind(ga);
        let gb = a.new_label();
        a.cmp_imm_w(10, 6);
        a.b_cond(C_LO, gb);
        a.ldur(13, 20, -16);
        a.ldur(15, 13, strong);
        a.cmp_imm_x(15, 1);
        a.b_cond(C_LS, slow);
        a.bind(gb);
        let da = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, da);
        a.sub_imm(14, 14, 1);
        a.stur(14, 12, strong);
        a.bind(da);
        let db = a.new_label();
        a.cmp_imm_w(10, 6);
        a.b_cond(C_LO, db);
        a.sub_imm(15, 15, 1);
        a.stur(15, 13, strong);
        a.bind(db);
        a.b(l_false);
    }
    a.bind(l_num);
    a.ldur_d(0, 20, -40);
    a.ldur_d(1, 20, -16);
    if let Some(target) = branch {
        // Straight-line fused numeric compare — branch on the negated condition, matching the
        // ordered-relation fusion (IEEE unordered must jump for == and fall through for !=).
        a.sub_imm(20, 20, 48);
        a.fcmp(0, 1);
        a.b_cond(if negate { C_EQ } else { C_NE }, target);
        a.b(done);
    } else {
        a.fcmp(0, 1);
        a.cset_w(11, C_EQ); // unordered (NaN) → 0: correctly unequal
        a.b(l_have);
    }
    a.bind(l_true);
    a.movz(11, 1, 0);
    a.b(l_have);
    a.bind(l_false);
    a.movz(11, 0, 0);
    a.bind(l_have);
    a.sub_imm(20, 20, 48);
    match branch {
        Some(target) => {
            // JumpIfFalse jumps when `eq ^ negate` is 0 — fold the negate into branch polarity.
            if negate {
                a.cbnz(11, false, target);
            } else {
                a.cbz(11, false, target);
            }
            a.b(done);
        }
        None => {
            if negate {
                a.movz(12, 1, 0);
                a.logic_w(2, 11, 11, 12); // eor: flip the pushed bool
            }
            a.movz(10, 3, 0); // Bool tag word (payload byte 1 patched below)
            a.stur(10, 20, 0);
            a.sturb(11, 20, 1);
            a.add_imm(20, 20, 24);
            a.b(done);
        }
    }
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    if let Some(target) = branch {
        // Unfused fallback: generic compare (pushes a bool), then pop-and-branch.
        emit_cond(a, COND_POP_TRUTHY, l_unwind);
        a.cbz(1, false, target);
    }
    a.bind(done);
}

/// Inline `!x` (ToBoolean + negate): Bool flips its payload; a Number is falsy iff ±0 or NaN;
/// undefined/null are falsy; a Str is falsy iff empty (length read through the header); Sym/Obj
/// are truthy — except a possible `[[IsHTMLDDA]]` object, so the Obj arm requires the
/// receiver's `ic_plain` byte. BigInt and any refcounted operand that is a last reference take
/// the helper. Guards all branch to `slow` before any state is written.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_not_inline(a: &mut asm::Asm, layout: &crate::value::JitLayout, pc: u32, l_unwind: usize) {
    let strong = layout.rc_strong_off as i32;
    let len_off = crate::lstr::LEN_OFF as u32;
    let slow = a.new_label();
    let done = a.new_label();
    let l_bool = a.new_label();
    let l_num = a.new_label();
    let l_str = a.new_label();
    let l_objsym = a.new_label();
    let l_true = a.new_label();
    let l_have = a.new_label();
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_LS, l_true); // undefined/null → !falsy = true
    a.cmp_imm_w(9, 3);
    a.b_cond(C_EQ, l_bool);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_EQ, l_num);
    a.cmp_imm_w(9, 6);
    a.b_cond(C_EQ, l_str);
    a.cmp_imm_w(9, 7);
    a.b_cond(C_HS, l_objsym);
    a.b(slow); // BigInt
    a.bind(l_bool);
    a.ldurb(11, 20, -23);
    a.movz(12, 1, 0);
    a.logic_w(2, 11, 11, 12); // eor: flip
    a.b(l_have);
    a.bind(l_num);
    a.ldur_d(0, 20, -16);
    a.movz(12, 0, 0);
    a.fmov_d_x(1, 12); // d1 = +0.0
    a.fcmp(0, 1);
    a.cset_w(11, C_EQ); // ±0 → falsy
    a.cset_w(12, C_VS); // NaN (unordered) → falsy
    a.logic_w(1, 11, 11, 12); // orr
    a.b(l_have);
    a.bind(l_str);
    a.ldur(12, 20, -16);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow); // last reference: the drop runs a destructor
    a.ldr_w_imm(11, 12, len_off);
    a.cmp_imm_w(11, 0);
    a.cset_w(11, C_EQ); // empty → falsy
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.b(l_have);
    a.bind(l_objsym);
    a.ldur(12, 20, -16);
    let os_drop = a.new_label();
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, os_drop);
    // an Obj is only reliably truthy when it is ordinary (ic_plain rules out IsHTMLDDA)
    a.add_imm(11, 12, layout.obj_from_rc as u32);
    a.ldrb_imm(11, 11, layout.obj_ic_plain as u32);
    a.cbz(11, false, slow);
    a.bind(os_drop);
    a.ldur(14, 12, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow);
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.movz(11, 0, 0);
    a.b(l_have);
    a.bind(l_true);
    a.movz(11, 1, 0);
    a.bind(l_have);
    a.movz(10, 3, 0);
    a.stur(10, 20, -24);
    a.sturb(11, 20, -23);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Gate for the inline LoadName template: probed layouts hold and every baked offset fits its
/// instruction's immediate range.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn load_name_inlinable(layout: &crate::value::JitLayout) -> bool {
    // The global-mode path additionally bakes the property-IC offsets (shape/entries/accessor),
    // so it shares that gate.
    get_prop_inlinable(layout)
        && layout.rc_strong_off < 256
        && layout.scope_gen.is_multiple_of(4)
        && layout.scope_gen / 4 < 4096
        && layout.binding_value + 16 < 256
        && layout.binding_value < 4096
        && layout.binding_init < 4096
}

/// Inline free-name read (`LoadName`) against the per-site [`crate::bytecode::NameIc`]: compare
/// the live activation env pointer and the scope's binding-map generation, then copy the cached
/// binding's value straight out of the scope — no hashing, no helper call. The cache is filled
/// by the VM slow path (`Chunk::name_ic_fill`, depth-0 resolutions only); any mismatch — cold
/// cache, different env, structural scope change, TDZ, BigInt value — takes the checked helper.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_load_name_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
    // `LoadNameForCall`: the fast path pushes the `this` slot (Undefined — a depth-0 hit can't
    // come through a `with` object) below the value; the slow path runs the full op.
    for_call: bool,
) {
    let strong = layout.rc_strong_off as i32;
    let slow = a.new_label();
    let done = a.new_label();
    // Validate the cache and leave a pointer to the resolved Value in x14 (either mode).
    emit_name_ic_value_ptr(a, layout, cache_ptr, slow);
    // Value not a BigInt → copy the 24 bytes, bump if refcounted, push.
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    a.ldur(10, 14, 0);
    a.ldur(11, 14, 8);
    a.ldur(13, 14, 16);
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 11, strong);
    a.add_imm(16, 16, 1);
    a.stur(16, 11, strong);
    a.bind(nobump);
    if for_call {
        a.stur(31, 20, 0);
        a.stur(31, 20, 8);
        a.stur(31, 20, 16);
        a.add_imm(20, 20, 24);
    }
    a.stur(10, 20, 0);
    a.stur(11, 20, 8);
    a.stur(13, 20, 16);
    a.add_imm(20, 20, 24);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Shared LoadName cache validation: on success x14 points at the resolved `Value` (the binding's
/// value in scope mode, the global entry's value in global mode) and execution falls through; any
/// mismatch branches to `slow`. Clobbers x9-x17.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_name_ic_value_ptr(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    slow: usize,
) {
    use crate::bytecode::{NAME_IC_OFF_BINDING, NAME_IC_OFF_ENV, NAME_IC_OFF_GEN};
    let sg = layout.scope_gen as u32;
    let bv = layout.binding_value as u32;
    let bi = layout.binding_init as u32;
    let g_ex = layout.obj_exotic as u32;
    let g_sh = (layout.obj_props + layout.props_shape) as u32;
    let g_en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let g_ea = layout.entry_accessor as u32;
    let g_ev = layout.entry_value as u32;
    let g_es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;

    a.mov_imm64(12, cache_ptr as u64);
    a.ldr_imm(9, 19, 40); // ctx.env_raw
    a.ldr_imm(10, 12, NAME_IC_OFF_ENV);
    // Scope binding-map generation must be unchanged in both modes (a shadowing binding in the
    // start scope re-routes a global resolution too).
    a.ldr_w_imm(11, 9, sg);
    a.ldr_w_imm(13, 12, NAME_IC_OFF_GEN);
    a.cmp_reg_w(11, 13);
    a.b_cond(C_NE, slow);
    let scope = a.new_label();
    a.cmp_reg_x(9, 10);
    a.b_cond(C_EQ, scope);
    // --- global mode: ic.env == env|1 (env is ≥8-aligned, so +1 sets the tag bit) ---
    a.add_imm(11, 9, 1);
    a.cmp_reg_x(11, 10);
    a.b_cond(C_NE, slow);
    a.ldr_imm(14, 19, 56); // the realm's global Object
    a.ldrb_imm(15, 14, g_ex);
    a.cmp_imm_w(15, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(15, 14, layout.obj_ic_plain as u32); // not side-table masked
    a.cbz(15, false, slow);
    a.ldr_w_imm(15, 14, g_sh); // live shape vs cached (packed high half)
    a.ldr_imm(16, 12, NAME_IC_OFF_BINDING);
    a.lsr_imm(17, 16, 32);
    a.cmp_reg_w(15, 17);
    a.b_cond(C_NE, slow);
    a.mov_w(16, 16); // zero-extend the slot half
    a.ldr_imm(15, 14, g_en);
    a.mov_imm64(17, g_es);
    a.madd(15, 16, 17, 15);
    a.ldrb_imm(14, 15, g_ea);
    a.cbnz(14, false, slow);
    a.add_imm(14, 15, g_ev); // x14 → the entry's Value
    let have = a.new_label();
    a.b(have);
    // --- scope mode: binding initialized (TDZ) ---
    a.bind(scope);
    a.ldr_imm(14, 12, NAME_IC_OFF_BINDING);
    a.ldrb_imm(9, 14, bi);
    a.cbz(9, false, slow);
    a.add_imm(14, 14, bv); // x14 → the binding's Value
    a.bind(have);
}

/// Same gate as [`get_prop_inlinable`] plus the dense-element (`Props::elems`) and
/// writable-flag offsets the element templates bake in.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn elem_inlinable(layout: &crate::value::JitLayout) -> bool {
    let elp = layout.obj_props + layout.props_elems + layout.vec_ptr_off;
    let ell = layout.obj_props + layout.props_elems + layout.vec_len_off;
    get_prop_inlinable(layout)
        && elp.is_multiple_of(8)
        && elp / 8 < 4096
        && ell.is_multiple_of(8)
        && ell / 8 < 4096
        && layout.entry_writable < 4096
}

/// Inline dense-element read (`a[i]`): an own data element of a plain object/array, indexed
/// through `Props::elems` without hashing or stringifying the key — the machine-code mirror of
/// `Interp::fast_get_elem`. Every guard branches to `slow` before any state is written. Handles a
/// Num key that is exactly a u32 in dense bounds, a non-accessor slot, and a non-BigInt value on
/// a receiver that is not the last reference; the live `inline_ic_safe` flag rules out proxies /
/// typed arrays / module namespaces existing at all. Everything else falls to the checked helper.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_get_elem_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    pc: u32,
    l_unwind: usize,
) {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let elp = (layout.obj_props + layout.props_elems + layout.vec_ptr_off) as u32;
    let ell = (layout.obj_props + layout.props_elems + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. stack: [obj @ -48, key @ -24] — receiver must be Obj, key must be Num
    a.ldurb(9, 20, -48);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldurb(9, 20, -24);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    // 2. key must be exactly a u32 (round-trip compare; NaN/negative/fractional/huge all miss)
    a.ldur_d(0, 20, -16);
    a.fcvtzu_w_d(9, 0);
    a.ucvtf_d_w(1, 9);
    a.fcmp(0, 1);
    a.b_cond(C_NE, slow);
    // 3. receiver refcount > 1 (so the pop-drop below never frees)
    a.ldur(10, 20, -40);
    a.ldur(11, 10, strong);
    a.cmp_imm_x(11, 1);
    a.b_cond(C_LS, slow);
    // 4. object base; exotic must be None or Array, and plain (no side-table behavior)
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(12, 11, ex);
    let ex_ok = a.new_label();
    a.cmp_imm_w(12, none_tag);
    a.b_cond(C_EQ, ex_ok);
    a.cmp_imm_w(12, arr_tag);
    a.b_cond(C_NE, slow);
    a.bind(ex_ok);
    a.ldrb_imm(12, 11, plain);
    a.cbz(12, false, slow);
    // 5. dense bounds: n < elems.len (x9's upper bits are zero from the w-form fcvtzu)
    a.ldr_imm(12, 11, ell);
    a.cmp_reg_x(9, 12);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; NO_SLOT (0xFFFF_FFFF) = hole → slow
    a.ldr_imm(12, 11, elp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base = entries data ptr + slot*entry_size
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. not an accessor
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    // 9. value tag: BigInt (5) is a compound payload — helper. Others copy (+ bump for 6..8).
    a.ldurb(9, 15, ev);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    // --- commit: everything validated; from here only writes ---
    a.ldur(12, 15, ev);
    a.ldur(13, 15, ev + 8);
    a.ldur(14, 15, ev + 16);
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 13, strong);
    a.add_imm(16, 16, 1);
    a.stur(16, 13, strong);
    a.bind(nobump);
    // drop the receiver (strong was > 1; if the value IS the receiver the bump balanced it)
    a.ldur(9, 10, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, strong);
    // pop obj+key, push value → value lands at the obj slot, sp drops one
    a.stur(12, 20, -48);
    a.stur(13, 20, -40);
    a.stur(14, 20, -32);
    a.sub_imm(20, 20, 24);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Inline dense-element write (`a[i] = v`, and the value-keeping `SetElem` when `keep`): the
/// machine-code mirror of `Interp::fast_set_elem` — overwrite an existing own writable data
/// element. The old value drops inline (strong-- when refcounted and not the last reference);
/// `v` *moves* into the slot, so it needs no bump — except under `keep`, where it also stays on
/// the stack as the expression result and bumps once. A BigInt old value (compound drop), a
/// BigInt `v` under `keep` (compound clone), a last-reference old value or receiver, an accessor
/// or non-writable slot, or any dense miss falls to the checked helper.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_set_elem_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    pc: u32,
    l_unwind: usize,
    keep: bool,
) {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let elp = (layout.obj_props + layout.props_elems + layout.vec_ptr_off) as u32;
    let ell = (layout.obj_props + layout.props_elems + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. stack: [obj @ -72, key @ -48, v @ -24]
    a.ldurb(9, 20, -72);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldurb(9, 20, -48);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    if keep {
        // v is also the expression result: a BigInt can't clone inline.
        a.ldurb(9, 20, -24);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
    }
    // 2. key must be exactly a u32
    a.ldur_d(0, 20, -40);
    a.fcvtzu_w_d(9, 0);
    a.ucvtf_d_w(1, 9);
    a.fcmp(0, 1);
    a.b_cond(C_NE, slow);
    // 3. receiver refcount > 1
    a.ldur(10, 20, -64);
    a.ldur(11, 10, strong);
    a.cmp_imm_x(11, 1);
    a.b_cond(C_LS, slow);
    // 4. object base; exotic None or Array, and plain
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(12, 11, ex);
    let ex_ok = a.new_label();
    a.cmp_imm_w(12, none_tag);
    a.b_cond(C_EQ, ex_ok);
    a.cmp_imm_w(12, arr_tag);
    a.b_cond(C_NE, slow);
    a.bind(ex_ok);
    a.ldrb_imm(12, 11, plain);
    a.cbz(12, false, slow);
    // 5. dense bounds
    a.ldr_imm(12, 11, ell);
    a.cmp_reg_x(9, 12);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; hole → slow
    a.ldr_imm(12, 11, elp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. data property, writable
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    a.ldrb_imm(9, 15, ew);
    a.cbz(9, false, slow);
    // 9. old value: trivially droppable (tag ≤ 4), or refcounted with strong > 1 (inline dec);
    //    BigInt or a last reference → helper. An old value that IS the receiver (`a[0] === a`)
    //    also bails: its dec plus the receiver dec below would take the shared counter to 0
    //    without running the destructor. w9 = old tag, x12 = old payload, both live below.
    a.ldurb(9, 15, ev);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    let old_plain = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, old_plain);
    a.ldur(12, 15, ev + 8);
    a.cmp_reg_x(12, 10);
    a.b_cond(C_EQ, slow);
    a.ldur(13, 12, strong);
    a.cmp_imm_x(13, 1);
    a.b_cond(C_LS, slow);
    a.bind(old_plain);
    // --- commit ---
    // move v into the entry (24 bytes; a refcounted payload moves, not clones)
    a.ldur(14, 20, -24);
    a.ldur(16, 20, -16);
    a.ldur(17, 20, -8);
    a.stur(14, 15, ev);
    a.stur(16, 15, ev + 8);
    a.stur(17, 15, ev + 16);
    // drop the old value (refcounted: strong was > 1, so this never frees)
    let no_old_dec = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, no_old_dec);
    a.ldur(13, 12, strong);
    a.sub_imm(13, 13, 1);
    a.stur(13, 12, strong);
    a.bind(no_old_dec);
    if keep {
        // v now lives in the slot AND stays on the stack as the result: one bump.
        a.ldurb(9, 20, -24);
        let nb = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nb);
        a.ldur(13, 16, strong);
        a.add_imm(13, 13, 1);
        a.stur(13, 16, strong);
        a.bind(nb);
    }
    // drop the receiver (strong was > 1)
    a.ldur(13, 10, strong);
    a.sub_imm(13, 13, 1);
    a.stur(13, 10, strong);
    if keep {
        // [obj, key, v] → [v]: the result lands at the obj slot
        a.stur(14, 20, -72);
        a.stur(16, 20, -64);
        a.stur(17, 20, -56);
        a.sub_imm(20, 20, 48);
    } else {
        a.sub_imm(20, 20, 72);
    }
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Which fused parameter-slot element op to emit.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq)]
enum ElemLocalKind {
    /// `x[k]` → pops the key, pushes the element (net stack unchanged).
    Get,
    /// `x[k] = v` statement → pops key and value.
    SetDrop,
    /// `x[k] = v` expression → pops key and value, pushes `v` back.
    SetKeep,
}

/// Where a fused element read's key comes from.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq)]
enum KeySrc {
    /// On the operand stack (the plain op forms).
    Stack,
    /// Read straight from a local slot (peephole-fused `LoadLocal k; GetElemLocal x`).
    Slot(u32),
    /// Pre-increment/-decrement a numeric local slot in place and use the new value
    /// (peephole-fused `UpdateLocal(k, Pre*); GetElemLocal x`). The slot store is deferred to
    /// the commit point so a slow-path re-run never sees a half-applied update.
    SlotPre(u32, bool),
}

/// Inline fused element access where the receiver lives in a *parameter* slot
/// ([`crate::bytecode::Op::GetElemLocal`] and friends): like [`emit_get_elem_inline`] /
/// [`emit_set_elem_inline`] but the receiver is read straight out of the slot — it never crosses
/// the operand stack, so there is no receiver clone/drop refcounting at all (the slot's own
/// reference keeps it alive; no user code runs inside the fast path). A non-Obj slot (including
/// a defensive TDZ Empty) falls to the checked helper, which re-runs the op generically.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_elem_local_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    slot_off: u32,
    pc: u32,
    l_unwind: usize,
    kind: ElemLocalKind,
) {
    emit_elem_local_keyed(a, layout, slot_off, &[pc], l_unwind, kind, KeySrc::Stack);
}

/// [`emit_elem_local_inline`] parameterized on the key source (see [`KeySrc`]) — the peephole
/// pairs fuse the key-producing op into the element read, so their slow path re-runs *both*
/// original ops via the helper (`pcs` lists them in order; every guard runs before any state
/// is written, so the re-run is always clean).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_elem_local_keyed(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    slot_off: u32,
    pcs: &[u32],
    l_unwind: usize,
    kind: ElemLocalKind,
    key: KeySrc,
) {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let elp = (layout.obj_props + layout.props_elems + layout.vec_ptr_off) as u32;
    let ell = (layout.obj_props + layout.props_elems + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;
    let get = kind == ElemLocalKind::Get;
    debug_assert!(get || key == KeySrc::Stack);
    // Stack-keyed layout: Get → [key @ -24]; Set* → [key @ -48, v @ -24].
    let key_off = if get { -24 } else { -48 };

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. slot holds an Obj; key (from its source) is a Num, loaded into d0
    a.ldrb_imm(9, 22, slot_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    match key {
        KeySrc::Stack => {
            a.ldurb(9, 20, key_off);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, slow);
            a.ldur_d(0, 20, key_off + 8);
        }
        KeySrc::Slot(k_off) => {
            a.ldrb_imm(9, 22, k_off);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, slow);
            a.ldr_d_imm(0, 22, k_off + 8);
        }
        KeySrc::SlotPre(k_off, dec) => {
            a.ldrb_imm(9, 22, k_off);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, slow);
            a.ldr_d_imm(0, 22, k_off + 8);
            a.fmov_one(1);
            a.f_arith(if dec { 1 } else { 0 }, 0, 0, 1); // d0 = slot ± 1 (store deferred)
        }
    }
    // 2. key must be exactly a u32
    a.fcvtzu_w_d(9, 0);
    a.ucvtf_d_w(1, 9);
    a.fcmp(0, 1);
    a.b_cond(C_NE, slow);
    // 3. receiver rc ptr straight from the slot (no strong-count games — nothing drops)
    a.ldr_imm(10, 22, slot_off + 8);
    // 4. object base; exotic None or Array, and plain
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(12, 11, ex);
    let ex_ok = a.new_label();
    a.cmp_imm_w(12, none_tag);
    a.b_cond(C_EQ, ex_ok);
    a.cmp_imm_w(12, arr_tag);
    a.b_cond(C_NE, slow);
    a.bind(ex_ok);
    a.ldrb_imm(12, 11, plain);
    a.cbz(12, false, slow);
    // 5. dense bounds
    a.ldr_imm(12, 11, ell);
    a.cmp_reg_x(9, 12);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; hole → slow
    a.ldr_imm(12, 11, elp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. data property (+ writable for the set forms)
    a.ldrb_imm(9, 15, ea);
    a.cbnz(9, false, slow);
    if get {
        // 9. value tag: BigInt → helper; commit: copy + bump, then place the result
        a.ldurb(9, 15, ev);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.ldur(12, 15, ev);
        a.ldur(13, 15, ev + 8);
        a.ldur(14, 15, ev + 16);
        let nobump = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nobump);
        a.ldur(16, 13, strong);
        a.add_imm(16, 16, 1);
        a.stur(16, 13, strong);
        a.bind(nobump);
        match key {
            KeySrc::Stack => {
                // pop key, push value → result replaces the key slot
                a.stur(12, 20, -24);
                a.stur(13, 20, -16);
                a.stur(14, 20, -8);
            }
            KeySrc::Slot(_) | KeySrc::SlotPre(..) => {
                if let KeySrc::SlotPre(k_off, _) = key {
                    a.str_d_imm(0, 22, k_off + 8); // commit the deferred ±1 to the slot
                }
                // nothing was on the stack: push the value
                a.stur(12, 20, 0);
                a.stur(13, 20, 8);
                a.stur(14, 20, 16);
                a.add_imm(20, 20, 24);
            }
        }
    } else {
        a.ldrb_imm(9, 15, ew);
        a.cbz(9, false, slow);
        if kind == ElemLocalKind::SetKeep {
            // v is also the expression result: a BigInt can't clone inline.
            a.ldurb(9, 20, -24);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_EQ, slow);
        }
        // 9. old value: trivially droppable, or refcounted with strong > 1
        a.ldurb(9, 15, ev);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        let old_plain = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, old_plain);
        a.ldur(12, 15, ev + 8);
        a.ldur(13, 12, strong);
        a.cmp_imm_x(13, 1);
        a.b_cond(C_LS, slow);
        a.bind(old_plain);
        // --- commit: move v into the entry, drop the old value ---
        a.ldur(14, 20, -24);
        a.ldur(16, 20, -16);
        a.ldur(17, 20, -8);
        a.stur(14, 15, ev);
        a.stur(16, 15, ev + 8);
        a.stur(17, 15, ev + 16);
        let no_old_dec = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, no_old_dec);
        a.ldur(13, 12, strong);
        a.sub_imm(13, 13, 1);
        a.stur(13, 12, strong);
        a.bind(no_old_dec);
        if kind == ElemLocalKind::SetKeep {
            // v now lives in the slot AND stays on the stack: one bump, result at the key slot.
            a.ldurb(9, 20, -24);
            let nb = a.new_label();
            a.cmp_imm_w(9, 6);
            a.b_cond(C_LO, nb);
            a.ldur(13, 16, strong);
            a.add_imm(13, 13, 1);
            a.stur(13, 16, strong);
            a.bind(nb);
            a.stur(14, 20, -48);
            a.stur(16, 20, -40);
            a.stur(17, 20, -32);
            a.sub_imm(20, 20, 24);
        } else {
            a.sub_imm(20, 20, 48);
        }
    }
    a.b(done);
    a.bind(slow);
    for &p in pcs {
        emit_exec(a, p, l_unwind);
    }
    a.bind(done);
}

/// One op of a numeric register chain (see [`build_chain`]). Every value the chain produces is a
/// proven Num held in a callee-saved FP register (d8..d15) instead of the operand stack.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[derive(Clone, Copy)]
enum ChainOp {
    /// Push a Num constant (f64 bits).
    ConstNum(u64),
    /// Push a numeric local (slot byte offset).
    Load(u32),
    /// `++`/`--` a numeric local in place (slot byte offset); pushes per the kind.
    Update(u32, UpdKind),
    /// Dense element read: virtual key → virtual Num element (receiver slot byte offset).
    GetElem(u32),
    /// Dense element write from virtual `[key, v]` (receiver slot byte offset); `true` = keep
    /// `v` as the virtual result (`SetElemLocal` vs `SetElemLocalDrop`).
    SetElem(u32, bool),
    /// fadd/fsub/fmul/fdiv on the two virtual tops (same encoding as [`asm::Asm::f_arith`]).
    Arith(u32),
    /// Int32 op on the two virtual tops: 0=and 1=or 2=xor 3=shl 4=ushr 5=shr. Operands convert
    /// via guarded ToInt32 (guard-free when the virtual is known int-valued); the result is a
    /// known int-valued Num.
    Bit(u32),
    Neg,
    /// Store the virtual top into a local slot (byte offset).
    Store(u32),
    Pop,
    /// Duplicate the virtual top (compound element assignment's key copy).
    Dup,
    /// `ToPropKeyLocal` on an in-chain key: a proven Num needs no coercion — pure nop.
    KeyNop,
    /// Cached free-name read that must currently hold a Num (the `NameIc` cell address).
    LoadName(usize),
    /// Terminal fused compare+branch: negated ARM condition + target pc.
    CmpBranch(u32, usize),
}

/// Try to recognize a *numeric register chain* starting at `start`: a maximal run of ops whose
/// intermediate values can live entirely in FP registers — locals, dense elements, float
/// arithmetic, cached names — ending either naturally or in a fused compare+branch. Every op
/// consumes only values produced *within* the chain (tracked by `vdepth`), so each value is a
/// proven Num in a register: arithmetic needs no tag checks at all and the compare+branch needs
/// no guards whatsoever. Returns the chain and how many bytecode ops it covers (`None` if
/// shorter than 3 ops — plain templates are fine for those).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn build_chain(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    start: usize,
    targeted: &[bool],
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<(Vec<(ChainOp, usize)>, usize)> {
    use crate::bytecode::Op;
    let in_range = |s: u16| (s as u32) * 24 + 16 < 4096;
    let elem_ok = fast & 1024 != 0 && elem_inlinable(layout);
    let name_ok = fast & 8192 != 0 && load_name_inlinable(layout);
    let mut chain: Vec<(ChainOp, usize)> = Vec::new();
    let mut vdepth = 0usize;
    let mut pc = start;
    while pc < ops.len() {
        if pc > start && targeted[pc] {
            break; // a jump lands here: the canonical (memory) stack state must hold
        }
        let (op, push, pop): (ChainOp, usize, usize) = match &ops[pc] {
            Op::Const(k) => match chunk.jit_const_num(*k) {
                Some(bits) => (ChainOp::ConstNum(bits), 1, 0),
                None => break,
            },
            Op::LoadLocal(s) if in_range(*s) => (ChainOp::Load(*s as u32 * 24), 1, 0),
            Op::UpdateLocal(s, kind) if in_range(*s) => {
                let pushes = !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard);
                (ChainOp::Update(*s as u32 * 24, *kind), pushes as usize, 0)
            }
            Op::GetElemLocal(x) if elem_ok && in_range(*x) && vdepth >= 1 => {
                (ChainOp::GetElem(*x as u32 * 24), 1, 1)
            }
            Op::SetElemLocal(x) if elem_ok && in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 24, true), 1, 2)
            }
            Op::SetElemLocalDrop(x) if elem_ok && in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 24, false), 0, 2)
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div if vdepth >= 2 => {
                let f = match ops[pc] {
                    Op::Add => 0,
                    Op::Sub => 1,
                    Op::Mul => 2,
                    _ => 3,
                };
                (ChainOp::Arith(f), 1, 2)
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
                if vdepth >= 2 =>
            {
                let code = match ops[pc] {
                    Op::BitAnd => 0,
                    Op::BitOr => 1,
                    Op::BitXor => 2,
                    Op::Shl => 3,
                    Op::UShr => 4,
                    _ => 5, // Shr
                };
                (ChainOp::Bit(code), 1, 2)
            }
            Op::Neg if vdepth >= 1 => (ChainOp::Neg, 1, 1),
            Op::StoreLocal(s) if in_range(*s) => {
                if vdepth >= 1 {
                    (ChainOp::Store(*s as u32 * 24), 0, 1)
                } else {
                    break;
                }
            }
            Op::Pop if vdepth >= 1 => (ChainOp::Pop, 0, 1),
            Op::Dup if vdepth >= 1 => (ChainOp::Dup, 1, 0),
            Op::ToPropKeyLocal(_) if vdepth >= 1 => (ChainOp::KeyNop, 0, 0),
            Op::LoadName(_, c) if name_ok => {
                (ChainOp::LoadName(chunk.jit_name_cache_ptr(*c)), 1, 0)
            }
            Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::StrictEq | Op::StrictNotEq | Op::EqEq
            | Op::NotEq
                if vdepth == 2 =>
            {
                match ops.get(pc + 1) {
                    Some(Op::JumpIfFalse(t)) if !targeted[pc + 1] => {
                        let neg = match ops[pc] {
                            Op::Lt => 5,  // PL (unordered jumps)
                            Op::Gt => 13, // LE
                            Op::Le => 8,  // HI
                            Op::Ge => 11, // LT
                            Op::StrictEq | Op::EqEq => 1, // NE
                            _ => 0,       // EQ
                        };
                        chain.push((ChainOp::CmpBranch(neg, *t as usize), pc));
                    }
                    _ => {}
                }
                break;
            }
            _ => break,
        };
        if vdepth - pop + push > 8 {
            break; // out of d-registers
        }
        vdepth = vdepth - pop + push;
        chain.push((op, pc));
        pc += 1;
    }
    // Trim trailing pure producers: a Load/Const/LoadName whose value nothing in the chain
    // consumes would only be spilled back to the stack — zero benefit, and for an *object*
    // local (an array receiver feeding a non-chain GetElem/SetElem) the Num guard would fail
    // every execution, sending the whole bail tail through the generic helper. Emitting them
    // as plain templates instead is both faster and type-agnostic.
    while matches!(
        chain.last(),
        Some((ChainOp::ConstNum(_) | ChainOp::Load(_) | ChainOp::LoadName(_), _))
    ) {
        chain.pop();
    }
    // Same idea anywhere in the chain: a pure producer whose value nothing in the chain consumes
    // (a call argument, an array receiver below the real work — `x.am(i, a[i], r, 2*i, 0, 1)`)
    // would only be spilled — and when the value is an object, its Num guard fails every single
    // execution, condemning the whole tail to the generic helper. Cut the chain just before the
    // earliest such producer; the main loop emits it as a plain template and re-attempts a chain
    // right after it. Iterate: each cut can orphan earlier consumers.
    loop {
        let mut sim: Vec<usize> = Vec::new();
        for (idx, &(op, _)) in chain.iter().enumerate() {
            let (pops, pushes): (usize, usize) = match op {
                ChainOp::ConstNum(_) | ChainOp::Load(_) | ChainOp::LoadName(_) => (0, 1),
                ChainOp::Update(_, k) => (
                    0,
                    !matches!(k, UpdKind::IncDiscard | UpdKind::DecDiscard) as usize,
                ),
                ChainOp::GetElem(_) => (1, 1),
                ChainOp::SetElem(_, keep) => (2, keep as usize),
                ChainOp::Arith(_) | ChainOp::Bit(_) => (2, 1),
                ChainOp::Neg => (1, 1),
                ChainOp::Store(_) | ChainOp::Pop => (1, 0),
                ChainOp::Dup => (0, 1),
                ChainOp::KeyNop => (0, 0),
                ChainOp::CmpBranch(..) => (2, 0),
            };
            for _ in 0..pops {
                sim.pop();
            }
            for _ in 0..pushes {
                sim.push(idx);
            }
        }
        let cut = sim
            .iter()
            .copied()
            .filter(|&idx| {
                matches!(
                    chain[idx].0,
                    ChainOp::ConstNum(_) | ChainOp::Load(_) | ChainOp::LoadName(_)
                )
            })
            .min();
        match cut {
            Some(idx) => chain.truncate(idx),
            None => break,
        }
        if chain.is_empty() {
            return None;
        }
    }
    if chain.len() < 3 {
        return None;
    }
    let consumed = chain.last().map_or(0, |&(op, p)| {
        p - start + if matches!(op, ChainOp::CmpBranch(..)) { 2 } else { 1 }
    });
    Some((chain, consumed))
}

/// Emit a numeric register chain (see [`build_chain`]): the virtual operand stack lives in
/// d8..d15 (callee-saved — the prologue preserves them), scratch math uses d0..d3. Any guard
/// failure spills the virtual values to the real operand stack — in stack order, exactly the
/// state the ops would have produced — and re-runs the failing op and everything after it
/// through the generic helper, so semantics are identical on every path. Side-effecting ops
/// (slot stores, element writes) commit only after all their guards pass, which is what makes
/// the spill-and-rerun always clean.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_chain(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    chain: &[(ChainOp, usize)],
    pc_labels: &[usize],
    l_unwind: usize,
) {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let elp = (layout.obj_props + layout.props_elems + layout.vec_ptr_off) as u32;
    let ell = (layout.obj_props + layout.props_elems + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;
    let plain = layout.obj_ic_plain as u32;

    let done = a.new_label();
    // Virtual stack: (d-register, known-int-valued). Int-valued means the f64 is integral and in
    // i64 range, so a ToInt32 conversion is a bare fcvtzs with no round-trip guard.
    let mut vregs: Vec<(u32, bool)> = Vec::new();
    let mut free: Vec<u32> = vec![15, 14, 13, 12, 11, 10, 9, 8];
    // Receiver cache: (slot byte offset → x-register holding the validated Object base). The
    // chain fast path calls no helpers, so between element ops nothing can change the slot's
    // tag, the object's exotic status, or the ic-safe flag — the first access per receiver
    // validates and later ones just reuse the base. Invalidation: an in-chain Store/Update to
    // the slot drops its entry; LoadName clobbers all scratch registers, so it flushes.
    let mut rcache: Vec<(u32, u32)> = Vec::new();
    let mut rfree: Vec<u32> = vec![17, 16];
    // (chain index, bail label, virtual stack *before* the op) — slow paths follow the fast body.
    let mut bails: Vec<(usize, usize, Vec<(u32, bool)>)> = Vec::new();

    for (idx, (cop, _pc)) in chain.iter().enumerate() {
        // One bail label per chain op. The snapshot is the virtual stack before the op runs: the
        // emitter pops from `vregs` up front, but every guard fires before the op writes any
        // register or memory, so the snapshot registers still hold the pre-op values at any bail.
        let bail = a.new_label();
        let pre_op: Vec<(u32, bool)> = vregs.clone();
        let mut used = 0u32;
        macro_rules! guard {
            () => {{
                used += 1;
                bail
            }};
        }
        match *cop {
            ChainOp::ConstNum(bits) => {
                let rd = free.pop().expect("chain reg underflow");
                a.mov_imm64(9, bits);
                a.fmov_d_x(rd, 9);
                let f = f64::from_bits(bits);
                let iv =
                    f.fract() == 0.0 && (-9.223372036854776e18..9.223372036854776e18).contains(&f);
                vregs.push((rd, iv));
            }
            ChainOp::Load(off) => {
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, guard!());
                let rd = free.pop().expect("chain reg underflow");
                a.ldr_d_imm(rd, 22, off + 8);
                vregs.push((rd, false));
            }
            ChainOp::Update(off, kind) => {
                if let Some(k) = rcache.iter().position(|c| c.0 == off) {
                    let (_, breg) = rcache.remove(k);
                    rfree.push(breg);
                }
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, guard!());
                let dec = matches!(
                    kind,
                    UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
                );
                let f = if dec { 1 } else { 0 };
                match kind {
                    UpdKind::PreInc | UpdKind::PreDec => {
                        let rd = free.pop().expect("chain reg underflow");
                        a.ldr_d_imm(rd, 22, off + 8);
                        a.fmov_one(0);
                        a.f_arith(f, rd, rd, 0);
                        a.str_d_imm(rd, 22, off + 8);
                        vregs.push((rd, false));
                    }
                    UpdKind::PostInc | UpdKind::PostDec => {
                        let rd = free.pop().expect("chain reg underflow");
                        a.ldr_d_imm(rd, 22, off + 8);
                        a.fmov_one(0);
                        a.f_arith(f, 1, rd, 0);
                        a.str_d_imm(1, 22, off + 8);
                        vregs.push((rd, false)); // the old value is the result
                    }
                    UpdKind::IncDiscard | UpdKind::DecDiscard => {
                        a.ldr_d_imm(0, 22, off + 8);
                        a.fmov_one(1);
                        a.f_arith(f, 0, 0, 1);
                        a.str_d_imm(0, 22, off + 8);
                    }
                }
            }
            ChainOp::GetElem(xoff) | ChainOp::SetElem(xoff, _) => {
                let is_set = matches!(*cop, ChainOp::SetElem(..));
                let keep = matches!(*cop, ChainOp::SetElem(_, true));
                let (dv, viv) = if is_set {
                    vregs.pop().expect("chain vstack")
                } else {
                    (0, false)
                };
                let (dk, _) = vregs.pop().expect("chain vstack");
                // key is exactly a u32
                a.fcvtzu_w_d(9, dk);
                a.ucvtf_d_w(0, 9);
                a.fcmp(dk, 0);
                a.b_cond(C_NE, guard!());
                match rcache.iter().find(|c| c.0 == xoff) {
                    Some(&(_, breg)) => a.mov(11, breg),
                    None => {
                        // First access to this receiver in the chain: validate once.
                        a.ldrb_imm(10, 22, xoff); // slot holds an Obj
                        a.cmp_imm_w(10, 8);
                        a.b_cond(C_NE, guard!());
                        a.ldr_imm(10, 22, xoff + 8);
                        a.add_imm(11, 10, rcv);
                        a.ldrb_imm(12, 11, ex);
                        let ex_ok = a.new_label();
                        a.cmp_imm_w(12, none_tag);
                        a.b_cond(C_EQ, ex_ok);
                        a.cmp_imm_w(12, arr_tag);
                        a.b_cond(C_NE, guard!());
                        a.bind(ex_ok);
                        a.ldrb_imm(12, 11, plain); // no side-table behavior
                        a.cbz(12, false, guard!());
                        if let Some(breg) = rfree.pop() {
                            a.mov(breg, 11);
                            rcache.push((xoff, breg));
                        }
                    }
                }
                a.ldr_imm(12, 11, ell);
                a.cmp_reg_x(9, 12);
                a.b_cond(C_HS, guard!());
                a.ldr_imm(12, 11, elp);
                a.add_shifted(12, 12, 9, 2);
                a.ldr_w_imm(13, 12, 0);
                a.cmn_imm_w(13, 1);
                a.b_cond(C_EQ, guard!());
                a.ldr_imm(15, 11, en);
                a.movz(9, es as u32, 0); // entry stride (< 65536; the key index in x9 is dead)
                a.madd(15, 13, 9, 15);
                a.ldrb_imm(9, 15, ea);
                a.cbnz(9, false, guard!());
                if is_set {
                    a.ldrb_imm(9, 15, ew);
                    a.cbz(9, false, guard!());
                    // old value: droppable inline, or bail (w14/x12 stay live to the dec)
                    a.ldrb_imm(14, 15, ev as u32);
                    a.cmp_imm_w(14, 5);
                    a.b_cond(C_EQ, guard!());
                    let old_plain = a.new_label();
                    a.cmp_imm_w(14, 6);
                    a.b_cond(C_LO, old_plain);
                    a.ldur(12, 15, ev + 8);
                    a.ldur(13, 12, strong);
                    a.cmp_imm_x(13, 1);
                    a.b_cond(C_LS, guard!());
                    a.bind(old_plain);
                    // commit: entry = Num(dv); zero the third word; drop the old value
                    a.movz(9, 4, 0);
                    a.stur(9, 15, ev);
                    a.stur_d(dv, 15, ev + 8);
                    a.stur(31, 15, ev + 16);
                    let no_dec = a.new_label();
                    a.cmp_imm_w(14, 6);
                    a.b_cond(C_LO, no_dec);
                    a.ldur(13, 12, strong);
                    a.sub_imm(13, 13, 1);
                    a.stur(13, 12, strong);
                    a.bind(no_dec);
                    free.push(dk);
                    if keep {
                        vregs.push((dv, viv)); // v stays the virtual result (a Num — no refcounting)
                    } else {
                        free.push(dv);
                    }
                } else {
                    // element must be a Num to stay in a register
                    a.ldrb_imm(9, 15, ev as u32);
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_NE, guard!());
                    a.ldur_d(dk, 15, ev + 8); // reuse the key's register for the element
                    vregs.push((dk, false));
                }
            }
            ChainOp::Arith(f) => {
                let (rm, _) = vregs.pop().expect("chain vstack");
                let (rn, _) = vregs.pop().expect("chain vstack");
                a.f_arith(f, rn, rn, rm);
                vregs.push((rn, false));
                free.push(rm);
            }
            ChainOp::Bit(code) => {
                let (rm, mi) = vregs.pop().expect("chain vstack");
                let (rn, ni) = vregs.pop().expect("chain vstack");
                // ToInt32 each operand: fcvtzs truncates; the low 32 bits are the mod-2^32 wrap.
                // Known int-valued skips the round-trip guard (the conversion is exact by
                // construction); otherwise guard like the standalone template.
                for (src, iv, out) in [(rn, ni, 9u32), (rm, mi, 10u32)] {
                    a.fcvtzs_x_d(out, src);
                    if !iv {
                        a.scvtf_d_x(0, out);
                        a.frintz(1, src);
                        a.fcmp(0, 1);
                        a.b_cond(C_NE, guard!());
                        a.cmn_imm_x(out, 1);
                        a.b_cond(6, guard!()); // VS: the +2^63 saturation edge
                    }
                }
                match code {
                    0 => a.logic_w(0, 11, 9, 10),
                    1 => a.logic_w(1, 11, 9, 10),
                    2 => a.logic_w(2, 11, 9, 10),
                    3 => a.shift_w(0, 11, 9, 10),
                    4 => a.shift_w(1, 11, 9, 10),
                    _ => a.shift_w(2, 11, 9, 10),
                }
                if code == 4 {
                    a.ucvtf_d_w(rn, 11); // >>> yields an unsigned 32-bit result
                } else {
                    a.scvtf_d_w(rn, 11);
                }
                vregs.push((rn, true));
                free.push(rm);
            }
            ChainOp::Neg => {
                let (rt, _) = *vregs.last().expect("chain vstack");
                a.fneg(rt, rt);
                // Clear the int-valued flag: -(-2^63) = +2^63 escapes the guard-free i64 range.
                let top = vregs.len() - 1;
                vregs[top].1 = false;
            }
            ChainOp::Store(off) => {
                if let Some(k) = rcache.iter().position(|c| c.0 == off) {
                    let (_, breg) = rcache.remove(k);
                    rfree.push(breg);
                }
                let (dv, _) = vregs.pop().expect("chain vstack");
                // old slot value: trivially droppable, refcounted-and-shared (inline dec), or bail
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 5);
                a.b_cond(C_EQ, guard!());
                let plain = a.new_label();
                a.cmp_imm_w(9, 6);
                a.b_cond(C_LO, plain);
                a.ldr_imm(10, 22, off + 8);
                a.ldur(11, 10, strong);
                a.cmp_imm_x(11, 1);
                a.b_cond(C_LS, guard!());
                a.sub_imm(11, 11, 1);
                a.stur(11, 10, strong);
                a.bind(plain);
                a.movz(9, 4, 0);
                a.str_imm(9, 22, off);
                a.str_d_imm(dv, 22, off + 8);
                a.str_imm(31, 22, off + 16);
                free.push(dv);
            }
            ChainOp::Pop => {
                let (r, _) = vregs.pop().expect("chain vstack");
                free.push(r);
            }
            ChainOp::Dup => {
                let &(src, iv) = vregs.last().expect("chain vstack");
                let rd = free.pop().expect("chain reg underflow");
                a.fmov_d_d(rd, src);
                vregs.push((rd, iv));
            }
            ChainOp::KeyNop => {}
            ChainOp::LoadName(cache_ptr) => {
                // The validator clobbers x9-x17: every cached receiver base dies with it.
                for (_, breg) in rcache.drain(..) {
                    rfree.push(breg);
                }
                // Shared cache validation (scope or global mode) leaves x14 → the Value.
                emit_name_ic_value_ptr(a, layout, cache_ptr, guard!());
                a.ldurb(9, 14, 0);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, guard!()); // only a Num can live in a register
                let rd = free.pop().expect("chain reg underflow");
                a.ldur_d(rd, 14, 8);
                vregs.push((rd, false));
            }
            ChainOp::CmpBranch(neg, target) => {
                let (rm, _) = vregs.pop().expect("chain vstack");
                let (rn, _) = vregs.pop().expect("chain vstack");
                a.fcmp(rn, rm);
                a.b_cond(neg, pc_labels[target]);
                free.push(rm);
                free.push(rn);
            }
        }
        if used > 0 {
            bails.push((idx, bail, pre_op));
        }
    }
    // Chain finished: spill any remaining virtual values to the real stack, in stack order.
    for &(r, _) in &vregs {
        a.movz(9, 4, 0);
        a.stur(9, 20, 0);
        a.stur_d(r, 20, 8);
        a.stur(31, 20, 16);
        a.add_imm(20, 20, 24);
    }
    a.b(done);
    // ---- bail paths: spill the pre-op virtual stack, then re-run the rest via the helper ----
    for (idx, label, snap) in bails {
        a.bind(label);
        for &(r, _) in &snap {
            a.movz(9, 4, 0);
            a.stur(9, 20, 0);
            a.stur_d(r, 20, 8);
            a.stur(31, 20, 16);
            a.add_imm(20, 20, 24);
        }
        for (cop2, pc2) in &chain[idx..] {
            match cop2 {
                ChainOp::CmpBranch(_, target) => {
                    // generic compare (pushes a bool) + pop-and-branch, like the unfused pair
                    emit_exec(a, *pc2 as u32, l_unwind);
                    emit_cond(a, COND_POP_TRUTHY, l_unwind);
                    a.cbz(1, false, pc_labels[*target]);
                }
                _ => emit_exec(a, *pc2 as u32, l_unwind),
            }
        }
        a.b(done);
    }
    a.bind(done);
}

// ---------------------------------------------------------------------------------------------
// Loop-spanning chains: a fully-chainable, branch-free loop keeps its locals in registers
// across the back edge. Slot loads and type guards hoist into a one-time preamble; memory is
// written only on loop exit or on a bail, which flushes and jumps into the plain templates of
// the same region (still emitted as usual — the loop head's canonical label points at the chain
// entry, so both the fallthrough entry and plain back-edge jumps re-enter the chain). The loop
// is rotated: the condition runs once at entry (copy A, exits with nothing dirty) and again at
// the bottom of the body (copy B, exits through a flush), so the back edge is a single branch.
//
// Value kinds (decided by the planner, followed verbatim by the emitter):
//   K — compile-time f64 constant, materialized lazily (bit-op immediates are free)
//   I — exact integer in an x-register (x2..x8): keys and bit ops are single instructions;
//       float uses convert with one scvtf. `neg` = may be negative (sign-correctness matters).
//   D — f64 in a d-register (transients d16..; residents d8..d15); `iv` = proven integral with
//       |v| < 2^62, so ToInt32 is a bare fcvtzs with no round-trip guard.
//
// Residency: slots read before written preload behind a tag guard (a failed guard runs the
// whole loop through the plain templates); ±1-update targets whose stores stay integer live as
// I with a per-update magnitude guard that keeps them exact (JS numbers stop moving under ±1 at
// 2^53, so exceeding it must bail rather than diverge); everything else numeric lives as F.
// Slots written before read ("virgins") get no preamble load — a 2-instruction tag check bails
// to the plain loop if they hold a refcounted value, so every later flush is a plain overwrite.
// ---------------------------------------------------------------------------------------------

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
/// What a chain op pushes, precomputed by the planner (see the module comment above).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PushKind {
    None,
    K(u64),
    I { neg: bool },
    D { iv: bool },
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
/// Where a loop-touched numeric slot lives during the run.
#[derive(Clone, Copy, PartialEq, Debug)]
enum SlotRes {
    /// f64 home in a d-register (d8..d15).
    F(u32),
    /// Exact-integer home in an x-register (x2..x8).
    I(u32),
    /// Not register-resident: per-access guarded memory ops, like a plain chain.
    None,
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[derive(Debug)]
struct SlotPlan {
    off: u32,
    res: SlotRes,
    /// Read (or ±1-updated) before any region store: preamble tag-guard + load.
    preload: bool,
    /// Some Store/Update writes it in the region (it must flush on exits and bails).
    stored: bool,
    /// Stored before ever read: preamble checks the old value is refcount-free instead of
    /// loading it, so flushes can plain-overwrite.
    virgin: bool,
    /// F resident with a one-time exact-int entry check: loads carry `integral, |v| ≤ 2^31`,
    /// so integer arithmetic takes them with a bare fcvtzs.
    int_checked: bool,
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
struct LoopPlan {
    head: usize,
    jump_pc: usize,
    exit_pc: usize,
    /// Translated ops for `[head, jump_pc)`; the single CmpBranch ends the condition prefix.
    chain: Vec<(ChainOp, usize)>,
    /// Chain entries `[0, cond_len)` are the condition (emitted twice: entry + bottom).
    cond_len: usize,
    /// Per chain index: what the op pushes (kind agreement between planner and emitter).
    kinds: Vec<PushKind>,
    slots: Vec<SlotPlan>,
    /// Receiver slot offsets validated once into x16/x17.
    receivers: Vec<(u32, u32)>,
    /// GetElem chain idx → pin register holding its (guarded) result for later reuse.
    elem_retain: Vec<(usize, u32)>,
    /// GetElem chain idx → the retaining chain idx whose pin it copies from.
    elem_reuse: Vec<(usize, usize)>,
    /// Bit (chain idx, operand side) → pin register: retain the guarded ToInt32 result / reuse.
    conv_retain: Vec<((usize, u8), u32)>,
    conv_reuse: Vec<((usize, u8), u32)>,
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
/// Jump target of a control-flow op, if any.
fn op_jump_target(op: &crate::bytecode::Op) -> Option<usize> {
    use crate::bytecode::Op;
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfFalsePeek(t)
        | Op::JumpIfTruePeek(t)
        | Op::JumpIfNotNullishPeek(t)
        | Op::PushHandler(t) => Some(*t as usize),
        _ => None,
    }
}

/// Integer-range bookkeeping for iv decisions: |v| ≤ 2^exp and integral. 255 = unknown/not
/// integral. Kept crude on purpose — it only has to prove products/sums of masked values stay
/// under 2^62.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[derive(Clone, Copy)]
struct NumInfo {
    integral: bool,
    exp: u32,
    neg: bool,
}
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
impl NumInfo {
    fn unknown() -> NumInfo {
        NumInfo { integral: false, exp: 255, neg: true }
    }
    fn iv(&self) -> bool {
        self.integral && self.exp <= 62
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn plan_loop(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    targeted: &[bool],
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<LoopPlan> {
    use crate::bytecode::Op;
    if fast & 32768 == 0 {
        return None;
    }
    macro_rules! reject {
        ($why:expr) => {{
            if std::env::var_os("LUMEN_JIT_LOOPLOG").is_some() {
                eprintln!("[jit-loop] head {head}: reject: {}", $why);
            }
            return None;
        }};
    }
    let in_range = |s: u16| (s as u32) * 24 + 16 < 4096;

    // ---- region discovery: a unique back-edge Jump(head), nothing else targeting the interior
    let mut jump_pc = None;
    for (p, op) in ops.iter().enumerate() {
        if op_jump_target(op) == Some(head) {
            if matches!(op, Op::Jump(_)) && p > head && jump_pc.is_none() {
                jump_pc = Some(p);
            } else {
                return None;
            }
        }
    }
    let jump_pc = jump_pc?;
    if jump_pc == head + 1 {
        reject!("empty region");
    }
    for op in ops {
        if let Some(t) = op_jump_target(op) {
            if t > head && t <= jump_pc {
                reject!(format!("interior target {t}"));
            }
        }
    }
    debug_assert!(targeted[head]);

    // ---- translate the region; require full coverage and exactly one fused exit branch
    let mut chain: Vec<(ChainOp, usize)> = Vec::new();
    let mut vdepth = 0usize;
    let mut exit_pc = None;
    let mut cond_len = None;
    let mut pc = head;
    while pc < jump_pc {
        let (cop, push, pop): (ChainOp, usize, usize) = match &ops[pc] {
            Op::Const(k) => match chunk.jit_const_num(*k) {
                Some(bits) => (ChainOp::ConstNum(bits), 1, 0),
                None => return None,
            },
            Op::LoadLocal(s) if in_range(*s) => (ChainOp::Load(*s as u32 * 24), 1, 0),
            Op::UpdateLocal(s, kind) if in_range(*s) => {
                let pushes = !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard);
                (ChainOp::Update(*s as u32 * 24, *kind), pushes as usize, 0)
            }
            Op::GetElemLocal(x) if in_range(*x) && vdepth >= 1 => {
                (ChainOp::GetElem(*x as u32 * 24), 1, 1)
            }
            Op::SetElemLocal(x) if in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 24, true), 1, 2)
            }
            Op::SetElemLocalDrop(x) if in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 24, false), 0, 2)
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div if vdepth >= 2 => {
                let f = match ops[pc] {
                    Op::Add => 0,
                    Op::Sub => 1,
                    Op::Mul => 2,
                    _ => 3,
                };
                (ChainOp::Arith(f), 1, 2)
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
                if vdepth >= 2 =>
            {
                let code = match ops[pc] {
                    Op::BitAnd => 0,
                    Op::BitOr => 1,
                    Op::BitXor => 2,
                    Op::Shl => 3,
                    Op::UShr => 4,
                    _ => 5,
                };
                (ChainOp::Bit(code), 1, 2)
            }
            Op::Neg if vdepth >= 1 => (ChainOp::Neg, 1, 1),
            Op::StoreLocal(s) if in_range(*s) && vdepth >= 1 => {
                (ChainOp::Store(*s as u32 * 24), 0, 1)
            }
            Op::Pop if vdepth >= 1 => (ChainOp::Pop, 0, 1),
            Op::Dup if vdepth >= 1 => (ChainOp::Dup, 1, 0),
            Op::ToPropKeyLocal(_) if vdepth >= 1 => (ChainOp::KeyNop, 0, 0),
            Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::StrictEq | Op::StrictNotEq | Op::EqEq
            | Op::NotEq
                if vdepth == 2 =>
            {
                match ops.get(pc + 1) {
                    Some(Op::JumpIfFalse(t)) if (*t as usize) > jump_pc => {
                        if exit_pc.is_some() {
                            return None; // one exit only
                        }
                        let neg = match ops[pc] {
                            Op::Lt => 5,  // PL (unordered jumps)
                            Op::Gt => 13, // LE
                            Op::Le => 8,  // HI
                            Op::Ge => 11, // LT
                            Op::StrictEq | Op::EqEq => 1, // NE
                            _ => 0,       // EQ
                        };
                        exit_pc = Some(*t as usize);
                        chain.push((ChainOp::CmpBranch(neg, *t as usize), pc));
                        cond_len = Some(chain.len());
                        vdepth = 0;
                        pc += 2;
                        continue;
                    }
                    _ => return None,
                }
            }
            _ => reject!(format!("unchainable op at pc {pc}: {:?}", ops[pc])),
        };
        if vdepth - pop + push > 8 {
            reject!("vdepth > 8");
        }
        vdepth = vdepth - pop + push;
        chain.push((cop, pc));
        pc += 1;
    }
    let exit_pc = exit_pc?;
    let cond_len = cond_len?;
    if vdepth != 0 || cond_len == chain.len() {
        reject!("unbalanced or empty body");
    }

    // ---- value graph: per produced value, its consumers (for elem-int and residency choices)
    #[derive(Clone, Copy, PartialEq)]
    enum Use {
        Bit,
        Key,
        Cmp,
        Arith,
        Other,
    }
    let n = chain.len();
    // Node ids: one per chain index that pushes (Dup aliases its source).
    let mut consumers: Vec<Vec<Use>> = vec![Vec::new(); n];
    let mut slot_src: crate::fasthash::FastMap<u32, usize> = Default::default(); // off → node
    let mut slot_bind: crate::fasthash::FastMap<u32, usize> = Default::default();
    let mut stack: Vec<usize> = Vec::new();
    let mut elem_nodes: Vec<usize> = Vec::new(); // GetElem chain indices
    let mut receivers: Vec<u32> = Vec::new();
    let mut stored: Vec<u32> = Vec::new();
    let mut updated: Vec<u32> = Vec::new();
    // Raw memo inputs: element reads as (chain idx, receiver, key node), element writes as
    // (chain idx, receiver), bit ops as (chain idx, lhs node, rhs node).
    let mut elem_reads: Vec<(usize, u32, usize)> = Vec::new();
    let mut elem_writes: Vec<(usize, u32)> = Vec::new();
    let mut bit_uses: Vec<(usize, usize, usize)> = Vec::new();
    // Result → operand edges for the needs-int propagation below.
    let mut flow_edges: Vec<(usize, usize)> = Vec::new();
    for (idx, (cop, _)) in chain.iter().enumerate() {
        match *cop {
            ChainOp::ConstNum(_) => stack.push(idx),
            ChainOp::Load(off) => {
                let node = match slot_bind.get(&off) {
                    Some(&b) => b,
                    None => *slot_src.entry(off).or_insert(idx),
                };
                stack.push(node);
            }
            ChainOp::Update(off, kind) => {
                slot_src.entry(off).or_insert(idx);
                if !updated.contains(&off) {
                    updated.push(off);
                }
                if !stored.contains(&off) {
                    stored.push(off);
                }
                // The update's own read counts as an int-friendly use.
                let cur = slot_bind.get(&off).copied().or(slot_src.get(&off).copied());
                if let Some(c) = cur {
                    consumers[c].push(Use::Arith);
                }
                slot_bind.insert(off, idx);
                let pushes = !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard);
                if pushes {
                    // Post forms push the OLD value — the same node as the pre-update binding,
                    // so a later identical use (an element key, typically) can be deduplicated.
                    match kind {
                        UpdKind::PostInc | UpdKind::PostDec => stack.push(cur.unwrap_or(idx)),
                        _ => stack.push(idx),
                    }
                }
            }
            ChainOp::GetElem(xoff) => {
                let k = stack.pop().expect("loop plan stack");
                consumers[k].push(Use::Key);
                if !receivers.contains(&xoff) {
                    receivers.push(xoff);
                }
                elem_reads.push((idx, xoff, k));
                elem_nodes.push(idx);
                stack.push(idx);
            }
            ChainOp::SetElem(xoff, keep) => {
                let v = stack.pop().expect("loop plan stack");
                let k = stack.pop().expect("loop plan stack");
                consumers[v].push(Use::Other);
                consumers[k].push(Use::Key);
                if !receivers.contains(&xoff) {
                    receivers.push(xoff);
                }
                elem_writes.push((idx, xoff));
                if keep {
                    stack.push(v);
                }
            }
            ChainOp::Arith(_) => {
                let b = stack.pop().expect("loop plan stack");
                let a_ = stack.pop().expect("loop plan stack");
                consumers[a_].push(Use::Arith);
                consumers[b].push(Use::Arith);
                flow_edges.push((idx, a_));
                flow_edges.push((idx, b));
                stack.push(idx);
            }
            ChainOp::Bit(_) => {
                let b = stack.pop().expect("loop plan stack");
                let a_ = stack.pop().expect("loop plan stack");
                consumers[a_].push(Use::Bit);
                consumers[b].push(Use::Bit);
                bit_uses.push((idx, a_, b));
                stack.push(idx);
            }
            ChainOp::Neg => {
                let v = stack.pop().expect("loop plan stack");
                consumers[v].push(Use::Arith);
                flow_edges.push((idx, v));
                stack.push(idx);
            }
            ChainOp::Store(off) => {
                let v = stack.pop().expect("loop plan stack");
                consumers[v].push(Use::Other);
                slot_bind.insert(off, v);
                if !stored.contains(&off) {
                    stored.push(off);
                }
            }
            ChainOp::Pop => {
                let v = stack.pop().expect("loop plan stack");
                consumers[v].push(Use::Other);
            }
            ChainOp::Dup => {
                let v = *stack.last().expect("loop plan stack");
                stack.push(v);
            }
            ChainOp::KeyNop => {}
            ChainOp::CmpBranch(..) => {
                let b = stack.pop().expect("loop plan stack");
                let a_ = stack.pop().expect("loop plan stack");
                consumers[a_].push(Use::Cmp);
                consumers[b].push(Use::Cmp);
            }
            ChainOp::LoadName(_) => return None, // not supported in loop chains
        }
    }

    // Elem ops present require the inline layout; receivers must never be written in-region.
    if !elem_nodes.is_empty() || !receivers.is_empty() {
        if fast & 1024 == 0 || !elem_inlinable(layout) {
            reject!("elem layout");
        }
    }
    if receivers.len() > 2 {
        reject!("too many receivers");
    }
    for r in &receivers {
        if stored.contains(r) {
            reject!("stored receiver");
        }
    }

    // ---- slot classification
    let mut slot_offs: Vec<u32> = Vec::new();
    for (cop, _) in &chain {
        match *cop {
            ChainOp::Load(off) | ChainOp::Update(off, _) | ChainOp::Store(off) => {
                if !slot_offs.contains(&off) && !receivers.contains(&off) {
                    slot_offs.push(off);
                }
            }
            _ => {}
        }
    }
    // Read-before-store per slot: first access wins.
    let mut first_access: crate::fasthash::FastMap<u32, bool> = Default::default(); // true=read
    for (cop, _) in &chain {
        match *cop {
            ChainOp::Load(off) | ChainOp::Update(off, _) => {
                first_access.entry(off).or_insert(true);
            }
            ChainOp::Store(off) => {
                first_access.entry(off).or_insert(false);
            }
            _ => {}
        }
    }

    // needs-int: a value feeds a bit op or key, directly or through arithmetic whose result
    // does. This is what justifies speculative exact-int guards: a float here would have been
    // truncated (or bailed) downstream anyway, so proving int early only moves the check.
    let mut needs_int = vec![false; n];
    for (idx, uses) in consumers.iter().enumerate() {
        if uses.iter().any(|u| matches!(u, Use::Bit | Use::Key)) {
            needs_int[idx] = true;
        }
    }
    loop {
        let mut changed = false;
        for &(r, op) in &flow_edges {
            if needs_int[r] && !needs_int[op] {
                needs_int[op] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Elem-int decision: the value (transitively) feeds an int context.
    let elem_int: Vec<bool> = elem_nodes.iter().map(|&idx| needs_int[idx]).collect();

    // Residency policy. x-registers are scarce (9 shared with transients), so they go where
    // integer latency matters: counters (±1 updates), loop-carried accumulators (read before
    // stored — the cross-iteration critical path), and stored slots whose values feed bit ops
    // or keys directly. Read-only preloads that feed int contexts stay in d-registers behind a
    // one-time exact-int entry check (`int_checked`): integer arithmetic takes them with a bare
    // fcvtzs. The sim rounds below demote any I candidate whose stores turn out non-integer.
    let mut i_slots: Vec<u32> = updated.clone();
    let mut int_checked: Vec<u32> = Vec::new();
    // Store-value nodes per slot, for the direct-consumer test.
    let mut store_nodes: crate::fasthash::FastMap<u32, Vec<usize>> = Default::default();
    {
        let mut stack2: Vec<usize> = Vec::new();
        let mut bind2: crate::fasthash::FastMap<u32, usize> = Default::default();
        let mut src2: crate::fasthash::FastMap<u32, usize> = Default::default();
        for (idx, (cop, _)) in chain.iter().enumerate() {
            let (pops, pushes): (usize, usize) = match *cop {
                ChainOp::ConstNum(_) | ChainOp::LoadName(_) => (0, 1),
                ChainOp::Load(_) => (0, 1),
                ChainOp::Update(_, k) => (
                    0,
                    !matches!(k, UpdKind::IncDiscard | UpdKind::DecDiscard) as usize,
                ),
                ChainOp::GetElem(_) => (1, 1),
                ChainOp::SetElem(_, keep) => (2, keep as usize),
                ChainOp::Arith(_) | ChainOp::Bit(_) => (2, 1),
                ChainOp::Neg => (1, 1),
                ChainOp::Store(_) | ChainOp::Pop => (1, 0),
                ChainOp::Dup => (0, 1),
                ChainOp::KeyNop => (0, 0),
                ChainOp::CmpBranch(..) => (2, 0),
            };
            let mut popped: Vec<usize> = Vec::new();
            for _ in 0..pops {
                popped.push(stack2.pop().expect("residency stack"));
            }
            match *cop {
                ChainOp::Load(off) => {
                    let nd = bind2.get(&off).copied().unwrap_or_else(|| {
                        *src2.entry(off).or_insert(idx)
                    });
                    stack2.push(nd);
                }
                ChainOp::Update(off, kind) => {
                    let cur = bind2.get(&off).copied().or(src2.get(&off).copied());
                    src2.entry(off).or_insert(idx);
                    bind2.insert(off, idx);
                    if pushes == 1 {
                        match kind {
                            UpdKind::PostInc | UpdKind::PostDec => {
                                stack2.push(cur.unwrap_or(idx))
                            }
                            _ => stack2.push(idx),
                        }
                    }
                }
                ChainOp::Store(off) => {
                    store_nodes.entry(off).or_default().push(popped[0]);
                    bind2.insert(off, popped[0]);
                }
                ChainOp::Dup => {
                    let v = *stack2.last().expect("residency stack");
                    stack2.push(v);
                }
                ChainOp::SetElem(_, true) => stack2.push(popped[0]),
                _ => {
                    for _ in 0..pushes {
                        stack2.push(idx);
                    }
                }
            }
        }
    }
    for &off in &slot_offs {
        if i_slots.contains(&off) {
            continue;
        }
        let preloaded = first_access.get(&off).copied().unwrap_or(false);
        let is_stored = stored.contains(&off);
        if preloaded && !is_stored {
            if slot_src.get(&off).is_some_and(|&nd| needs_int[nd]) {
                int_checked.push(off);
            }
            continue;
        }
        if !is_stored {
            continue;
        }
        let carried = preloaded; // read before stored: loop-carried accumulator
        let bit_fed = store_nodes.get(&off).is_some_and(|nodes| {
            nodes.iter().any(|&nd| {
                consumers[nd]
                    .iter()
                    .any(|u| matches!(u, Use::Bit | Use::Key))
            })
        });
        if carried || bit_fed {
            i_slots.push(off);
        }
    }

    // ---- kind simulation (multiple rounds: residency demotions can change kinds, and the
    // loop-carried exponent bounds of int-resident slots need a cross-iteration fixed point)
    let mut plan_kinds: Vec<PushKind> = Vec::new();
    let mut i_peak = 0usize;
    let mut d_peak = 0usize;
    // Bit-operand kinds per (chain idx, side), from the final round (conversion memos below).
    let mut bit_kinds: crate::fasthash::FastMap<(usize, u8), PushKind> = Default::default();
    // Loop-head |value| ≤ 2^exp bound per int-resident slot: entry guards prove 31; stores
    // widen it; iterate until stable (or the slot demotes to float residency).
    let mut slot_exp_head: crate::fasthash::FastMap<u32, u32> = Default::default();
    for &off in &i_slots {
        slot_exp_head.insert(off, 31);
    }
    // One precise widening per slot; a second jumps past the int cap so the slot demotes and
    // the rounds terminate (a slot can otherwise creep +1 per round forever).
    let mut widened: Vec<u32> = Vec::new();
    #[allow(unused_assignments)]
    let mut stable = false;
    // Integer registers available to chains: x2..x8 plus x0/x1 — nothing in a chain fast path
    // calls out or scratches them (helpers only run on bail/exit stubs, after the flush).
    const I_UNIVERSE: [u32; 9] = [2, 3, 4, 5, 6, 7, 8, 0, 1];
    let use_count = |off: u32, chain: &[(ChainOp, usize)]| {
        chain
            .iter()
            .filter(|(c, _)| {
                matches!(*c, ChainOp::Load(o) | ChainOp::Update(o, _) | ChainOp::Store(o) if o == off)
            })
            .count()
    };
    // Whether an int-kind duplicate element read exists (it would want an x pin — worth
    // demoting one resident for, at ~20 instructions per iteration saved).
    let want_pin: usize = {
        let mut last: Vec<(u32, usize, bool)> = Vec::new();
        let mut dups = 0usize;
        let mut w = 0usize;
        for (k, &(idx, rcv, key)) in elem_reads.iter().enumerate() {
            while w < elem_writes.len() && elem_writes[w].0 < idx {
                last.clear();
                w += 1;
            }
            if last.iter().any(|&(r, kn, wi)| r == rcv && kn == key && wi == elem_int[k]) {
                if elem_int[k] {
                    dups += 1;
                }
            } else {
                last.push((rcv, key, elem_int[k]));
            }
        }
        dups.min(1)
    };
    let mut pin_demoted = false;
    'budget: loop {
        widened.clear();
        stable = false;
        for _round in 0..64 {
        plan_kinds = vec![PushKind::None; n];
        bit_kinds.clear();
        // (kind, info) per virtual value; slot state per off.
        let mut vstack: Vec<(PushKind, NumInfo)> = Vec::new();
        let mut slot_iv: crate::fasthash::FastMap<u32, NumInfo> = Default::default();
        for &off in &int_checked {
            slot_iv.insert(off, NumInfo { integral: true, exp: 31, neg: true });
        }
        let mut slot_exp: crate::fasthash::FastMap<u32, u32> = slot_exp_head.clone();
        let mut stored_exp: crate::fasthash::FastMap<u32, u32> = Default::default();
        let mut demote: Option<u32> = None;
        let mut i_live = 0usize;
        let mut d_live = 0usize;
        i_peak = 0;
        d_peak = 0;
        let mut elem_seen = 0usize;
        macro_rules! track {
            ($k:expr, $dir:tt) => {
                match $k {
                    PushKind::I { .. } => i_live = (i_live as isize $dir 1) as usize,
                    PushKind::D { .. } => d_live = (d_live as isize $dir 1) as usize,
                    _ => {}
                }
            };
        }
        for (idx, (cop, _)) in chain.iter().enumerate() {
            let (i_start, d_start) = (i_live, d_live);
            let mut i_pushed = 0usize;
            let mut d_pushed = 0usize;
            macro_rules! push {
                ($k:expr, $inf:expr) => {{
                    let (k, inf) = ($k, $inf);
                    track!(k, +);
                    match k {
                        PushKind::I { .. } => i_pushed += 1,
                        PushKind::D { .. } => d_pushed += 1,
                        _ => {}
                    }
                    plan_kinds[idx] = k;
                    vstack.push((k, inf));
                }};
            }
            macro_rules! pop {
                () => {{
                    let (k, inf) = vstack.pop().expect("loop kind stack");
                    track!(k, -);
                    (k, inf)
                }};
            }
            match *cop {
                ChainOp::ConstNum(bits) => {
                    let f = f64::from_bits(bits);
                    let integral = f.fract() == 0.0 && f.abs() < 9.0e18;
                    let exp = if integral {
                        (f.abs().max(1.0)).log2().ceil() as u32
                    } else {
                        255
                    };
                    push!(PushKind::K(bits), NumInfo { integral, exp, neg: f < 0.0 });
                }
                ChainOp::Load(off) => {
                    if i_slots.contains(&off) {
                        let exp = slot_exp.get(&off).copied().unwrap_or(31);
                        push!(
                            PushKind::I { neg: true },
                            NumInfo { integral: true, exp, neg: true }
                        );
                    } else {
                        let inf = slot_iv.get(&off).copied().unwrap_or(NumInfo::unknown());
                        push!(PushKind::D { iv: inf.iv() }, inf);
                    }
                }
                ChainOp::Update(off, kind) => {
                    if !i_slots.contains(&off) {
                        slot_iv.insert(off, NumInfo::unknown());
                    }
                    if !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard) {
                        if i_slots.contains(&off) {
                            push!(
                                PushKind::I { neg: true },
                                NumInfo { integral: true, exp: 31, neg: true }
                            );
                        } else {
                            push!(PushKind::D { iv: false }, NumInfo::unknown());
                        }
                    }
                }
                ChainOp::GetElem(_) => {
                    pop!();
                    let want_int = elem_int[elem_seen];
                    elem_seen += 1;
                    if want_int {
                        // The w-form conversion guard proves exact i32.
                        push!(
                            PushKind::I { neg: true },
                            NumInfo { integral: true, exp: 31, neg: true }
                        );
                    } else {
                        push!(PushKind::D { iv: false }, NumInfo::unknown());
                    }
                }
                ChainOp::SetElem(_, keep) => {
                    let (vk, vinf) = pop!();
                    pop!();
                    if keep {
                        push!(vk, vinf);
                    }
                }
                ChainOp::Arith(f) => {
                    let (bk, binf) = pop!();
                    let (ak, ainf) = pop!();
                    let integral = ainf.integral && binf.integral && f != 3;
                    let exp = match f {
                        0 | 1 => ainf.exp.max(binf.exp).saturating_add(1),
                        2 => ainf.exp.saturating_add(binf.exp),
                        _ => 255,
                    };
                    // Integer lowering: both operands are exact ints in registers (or int
                    // constants) and the result provably fits 2^52, so 64-bit integer add/sub/
                    // mul is exact and equals the f64 result — no guards, 1-cycle latency.
                    let int_side = |k: PushKind, inf: NumInfo| match k {
                        PushKind::I { .. } => true,
                        PushKind::K(_) => inf.integral && inf.exp <= 52,
                        // Proven-integral f64 (entry-checked preload or tracked store): a bare
                        // fcvtzs is exact.
                        PushKind::D { .. } => inf.integral && inf.exp <= 52,
                        _ => false,
                    };
                    if f != 3 && exp <= 52 && int_side(ak, ainf) && int_side(bk, binf) {
                        let neg = ainf.neg || binf.neg || f == 1;
                        push!(PushKind::I { neg }, NumInfo { integral: true, exp, neg });
                    } else {
                        let inf =
                            NumInfo { integral: integral && exp <= 62, exp, neg: true };
                        push!(PushKind::D { iv: inf.iv() }, inf);
                    }
                }
                ChainOp::Bit(code) => {
                    let (bk, binf) = pop!();
                    let (ak, ainf) = pop!();
                    let _ = binf;
                    bit_kinds.insert((idx, 0), ak);
                    bit_kinds.insert((idx, 1), bk);
                    let kbits = |k: PushKind| match k {
                        PushKind::K(b) => {
                            let f = f64::from_bits(b);
                            if f.fract() == 0.0 && (0.0..2147483648.0).contains(&f) {
                                Some(f as u32)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let inf = match code {
                        0 => {
                            // and: a nonneg constant mask bounds the result
                            match kbits(ak).into_iter().chain(kbits(bk)).min() {
                                Some(m) => NumInfo {
                                    integral: true,
                                    exp: 32 - m.leading_zeros(),
                                    neg: false,
                                },
                                None => NumInfo { integral: true, exp: 32, neg: true },
                            }
                        }
                        5 => {
                            // shr by a constant: |x >> k| ≤ max(|x| / 2^k, 1) with sign
                            // preserved (after the i32 wrap, so the input bound caps at 31).
                            match kbits(bk) {
                                Some(k) => {
                                    let e0 = ainf.exp.min(31);
                                    NumInfo {
                                        integral: true,
                                        exp: e0.saturating_sub(k.min(31)).max(1),
                                        neg: if ainf.exp <= 31 { ainf.neg } else { true },
                                    }
                                }
                                None => NumInfo { integral: true, exp: 32, neg: true },
                            }
                        }
                        3 => {
                            // shl by a constant of a small nonneg value can't wrap
                            match (kbits(bk), ainf.neg) {
                                (Some(k), false) if ainf.exp + k.min(31) <= 31 => NumInfo {
                                    integral: true,
                                    exp: ainf.exp + k.min(31),
                                    neg: false,
                                },
                                _ => NumInfo { integral: true, exp: 32, neg: true },
                            }
                        }
                        4 => NumInfo { integral: true, exp: 32, neg: false },
                        _ => NumInfo { integral: true, exp: 32, neg: true },
                    };
                    push!(PushKind::I { neg: inf.neg }, inf);
                }
                ChainOp::Neg => {
                    let (_, vinf) = pop!();
                    let inf = NumInfo { integral: vinf.integral, exp: vinf.exp, neg: true };
                    push!(PushKind::D { iv: inf.iv() }, inf);
                }
                ChainOp::Store(off) => {
                    let (vk, vinf) = pop!();
                    if i_slots.contains(&off) {
                        // A non-integer store demotes the slot: kinds must be re-simulated.
                        // Counter slots (±1 updates) additionally require i32 stores — the
                        // update sequence relies on the w-form overflow check.
                        let int_ok = match vk {
                            PushKind::I { .. } => true,
                            PushKind::K(b) => {
                                let f = f64::from_bits(b);
                                f.fract() == 0.0 && f.abs() < 9.0e15
                            }
                            _ => false,
                        };
                        let exp_cap = if updated.contains(&off) { 31 } else { 52 };
                        if (!int_ok || vinf.exp > exp_cap) && demote.is_none() {
                            demote = Some(off);
                        }
                        slot_exp.insert(off, vinf.exp);
                        let e = stored_exp.entry(off).or_insert(0);
                        *e = (*e).max(vinf.exp);
                    }
                    slot_iv.insert(off, vinf);
                }
                ChainOp::Pop => {
                    pop!();
                }
                ChainOp::Dup => {
                    let &(vk, vinf) = vstack.last().expect("loop kind stack");
                    push!(vk, vinf);
                }
                ChainOp::KeyNop => {}
                ChainOp::CmpBranch(..) => {
                    pop!();
                    pop!();
                }
                ChainOp::LoadName(_) => unreachable!(),
            }
            // Operand registers are freed only at op end, so an op needs its start-of-op
            // live set plus everything it pushes, simultaneously.
            i_peak = i_peak.max(i_live).max(i_start + i_pushed);
            d_peak = d_peak.max(d_live).max(d_start + d_pushed);
        }
        match demote {
            Some(off) => {
                if std::env::var_os("LUMEN_JIT_LOOPLOG").is_some() {
                    eprintln!("[jit-loop] head {head}: demote I slot {}", off / 24);
                }
                i_slots.retain(|&o| o != off);
                slot_exp_head.remove(&off);
            }
            None => {
                // Widen loop-head exponent bounds with what this round stored; a stable set of
                // bounds means the kinds are final.
                let mut changed = false;
                for (&off, &e) in &stored_exp {
                    if !i_slots.contains(&off) {
                        continue;
                    }
                    let entry = slot_exp_head.entry(off).or_insert(31);
                    let mut new = (*entry).max(e);
                    if new != *entry && widened.contains(&off) {
                        new = 53; // second widening: force the demotion path
                    }
                    if new != *entry {
                        *entry = new;
                        widened.push(off);
                        changed = true;
                    }
                }
                if !changed {
                    stable = true;
                    break;
                }
            }
        }
    }
        if !stable {
            reject!("kind rounds did not converge");
        }
        // Register budget: demote the least-used I resident and re-simulate when over, and
        // give up one more resident (once) if an elem-dup pin wants a register.
        let over = i_peak + i_slots.len() > I_UNIVERSE.len();
        let pin_squeeze =
            want_pin > 0 && !pin_demoted && i_peak + i_slots.len() == I_UNIVERSE.len();
        if over || pin_squeeze {
            let victim = i_slots
                .iter()
                .copied()
                .min_by_key(|&off| use_count(off, &chain));
            match victim {
                Some(v) => {
                    if std::env::var_os("LUMEN_JIT_LOOPLOG").is_some() {
                        eprintln!(
                            "[jit-loop] head {head}: demote I slot {} ({})",
                            v / 24,
                            if over { "pressure" } else { "pin" }
                        );
                    }
                    if pin_squeeze {
                        pin_demoted = true;
                    }
                    i_slots.retain(|&o| o != v);
                    slot_exp_head.remove(&v);
                    continue 'budget;
                }
                None => reject!(format!("i pressure: peak {i_peak}")),
            }
        }
        break;
    }
    if d_peak + 1 > 8 {
        reject!(format!("d pressure: peak {d_peak}"));
    }
    let f_slots: Vec<u32> = slot_offs
        .iter()
        .copied()
        .filter(|o| !i_slots.contains(o))
        .collect();
    if f_slots.len() > 8 {
        reject!(format!("f pressure: {} slots", f_slots.len()));
    }

    let mut slots: Vec<SlotPlan> = Vec::new();
    let mut next_d = 8u32;
    let mut next_x = 0usize; // index into I_UNIVERSE
    for &off in &slot_offs {
        let res = if i_slots.contains(&off) {
            let r = SlotRes::I(I_UNIVERSE[next_x]);
            next_x += 1;
            r
        } else if f_slots.contains(&off) {
            let r = SlotRes::F(next_d);
            next_d += 1;
            r
        } else {
            SlotRes::None
        };
        let preload = first_access.get(&off).copied().unwrap_or(false);
        let is_stored = stored.contains(&off);
        slots.push(SlotPlan {
            off,
            res,
            preload,
            stored: is_stored,
            virgin: is_stored && !preload,
            int_checked: int_checked.contains(&off),
        });
    }
    // Sanity: kinds recorded for the final residency sets. The last sim round used exactly
    // `i_slots`/all-resident F, matching the assignment above.

    let receivers: Vec<(u32, u32)> = receivers
        .iter()
        .enumerate()
        .map(|(k, &off)| (off, 16 + k as u32))
        .collect();

    // ---- memoization: duplicate element reads and repeated guarded ToInt32 conversions.
    // Node ids are SSA-like (an id never changes value), so a second element read with the same
    // (receiver, key id) — with no intervening element write — and a second Bit-op use of the
    // same unproven-f64 id can reuse the first result from a pinned register. Pins live in the
    // leftover resident registers; memos are dropped when none are free.
    // x pins: whatever the universe leaves after I residents and the transient reserve; d pins
    // from the resident bank's leftovers (d transients live in d16.. and never collide).
    let mut free_pin_x: Vec<u32> = I_UNIVERSE
        .iter()
        .copied()
        .filter(|x| !slots.iter().any(|s| s.res == SlotRes::I(*x)))
        .skip(i_peak)
        .collect();
    let mut free_pin_d: Vec<u32> = (next_d..16).collect();
    let mut elem_retain: Vec<(usize, u32)> = Vec::new();
    let mut elem_reuse: Vec<(usize, usize)> = Vec::new(); // (dup idx, retain idx)
    {
        // (rcv, key node, want-int) → retain chain idx
        let mut last: Vec<((u32, usize, bool), usize)> = Vec::new();
        let mut w = 0usize;
        for (k, &(idx, rcv, key)) in elem_reads.iter().enumerate() {
            // Any element write invalidates every pending read: two receiver slots can hold the
            // same array at runtime, so same-receiver screening would be unsound.
            while w < elem_writes.len() && elem_writes[w].0 < idx {
                last.clear();
                w += 1;
            }
            let want = elem_int[k];
            match last
                .iter()
                .find(|((r, kn, wi), _)| *r == rcv && *kn == key && *wi == want)
            {
                Some(&(_, ridx)) => elem_reuse.push((idx, ridx)),
                None => last.push(((rcv, key, want), idx)),
            }
        }
        // Only reads that are actually reused get pins.
        for &(_, ridx) in &elem_reuse {
            if !elem_retain.iter().any(|(i, _)| *i == ridx) {
                let k = elem_reads.iter().position(|&(i, _, _)| i == ridx).unwrap();
                let pin = if elem_int[k] { free_pin_x.pop() } else { free_pin_d.pop() };
                if let Some(r) = pin {
                    elem_retain.push((ridx, r));
                }
            }
        }
        // Drop reuses whose retain got no pin.
        elem_reuse.retain(|&(_, ridx)| elem_retain.iter().any(|(i, _)| *i == ridx));
    }
    let mut conv_retain: Vec<((usize, u8), u32)> = Vec::new();
    let mut conv_reuse: Vec<((usize, u8), u32)> = Vec::new();
    {
        // Guarded conversions only (D with iv=false): the 7-instruction guard is worth a pin.
        let mut by_id: crate::fasthash::FastMap<usize, Vec<(usize, u8)>> = Default::default();
        for &(idx, aid, bid) in &bit_uses {
            for (side, id) in [(0u8, aid), (1u8, bid)] {
                if matches!(bit_kinds.get(&(idx, side)), Some(PushKind::D { iv: false })) {
                    by_id.entry(id).or_default().push((idx, side));
                }
            }
        }
        let mut ids: Vec<(usize, Vec<(usize, u8)>)> =
            by_id.into_iter().filter(|(_, v)| v.len() >= 2).collect();
        ids.sort_by_key(|(id, _)| *id);
        for (_, mut uses) in ids {
            let Some(pin) = free_pin_x.pop() else { break };
            uses.sort();
            conv_retain.push((uses[0], pin));
            for &u in &uses[1..] {
                conv_reuse.push((u, pin));
            }
        }
    }

    if std::env::var_os("LUMEN_JIT_LOOPLOG").is_some() {
        eprintln!(
            "[jit-loop] head {head}: CHAINED {} ops, {} slots ({} I), {} receivers, memo elem {}r/{}u conv {}r/{}u",
            chain.len(),
            slots.len(),
            slots.iter().filter(|s| matches!(s.res, SlotRes::I(_))).count(),
            receivers.len(),
            elem_retain.len(),
            elem_reuse.len(),
            conv_retain.len(),
            conv_reuse.len()
        );
    }
    Some(LoopPlan {
        head,
        jump_pc,
        exit_pc,
        chain,
        cond_len,
        kinds: plan_kinds,
        slots,
        receivers,
        elem_retain,
        elem_reuse,
        conv_retain,
        conv_reuse,
    })
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
/// A virtual value during loop-chain emission.
#[derive(Clone, Copy)]
enum LV {
    K(u64),
    I(u32, bool), // x-register, may-be-negative
    D(u32, bool), // d-register, integral-valued
}

/// Emit the loop chain for `plan`. Returns the label for the plain fallback of the head op —
/// the caller binds it immediately after and continues emitting the plain region.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn emit_loop_chain(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &LoopPlan,
    pc_labels: &[usize],
) -> usize {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let elp = (layout.obj_props + layout.props_elems + layout.vec_ptr_off) as u32;
    let ell = (layout.obj_props + layout.props_elems + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;
    let plain = layout.obj_ic_plain as u32;

    let plain_h = a.new_label();
    let body_l = a.new_label();
    let exit_a = a.new_label();
    let exit_b = a.new_label();

    let slot = |off: u32| plan.slots.iter().find(|s| s.off == off);
    let rcv_reg = |off: u32| plan.receivers.iter().find(|r| r.0 == off).map(|r| r.1);
    // Virgins stored within the condition prefix (they flush even on the entry exit).
    let cond_virgins: Vec<u32> = plan.chain[..plan.cond_len]
        .iter()
        .filter_map(|(c, _)| match *c {
            ChainOp::Store(off) if slot(off).is_some_and(|s| s.virgin) => Some(off),
            _ => None,
        })
        .collect();
    let all_virgins: Vec<u32> =
        plan.slots.iter().filter(|s| s.virgin).map(|s| s.off).collect();

    // ---- preamble --------------------------------------------------------------------------
    for s in &plan.slots {
        if s.virgin {
            // The old value must be drop-free so flushes can plain-overwrite.
            a.ldrb_imm(9, 22, s.off);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_HS, plain_h);
        }
        if !s.preload {
            continue;
        }
        a.ldrb_imm(9, 22, s.off);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, plain_h);
        match s.res {
            SlotRes::F(d) => {
                a.ldr_d_imm(d, 22, s.off + 8);
                if s.int_checked {
                    // One-time exact-i32 proof; the value stays in its d home and integer
                    // consumers convert with a bare fcvtzs.
                    a.fcvtzs_w_d(9, d);
                    a.scvtf_d_w(1, 9);
                    a.fcmp(1, d);
                    a.b_cond(C_NE, plain_h);
                }
            }
            SlotRes::I(x) => {
                // Exact i32 (w-form conversion + compare-back): counters keep the invariant
                // with a flag-setting ±1, and the planner's range analysis starts from 2^31.
                a.ldr_d_imm(0, 22, s.off + 8);
                a.fcvtzs_w_d(x, 0);
                a.scvtf_d_w(1, x);
                a.fcmp(1, 0);
                a.b_cond(C_NE, plain_h);
                a.sxtw(x, x);
            }
            SlotRes::None => {}
        }
    }
    for &(off, r) in &plan.receivers {
        a.ldrb_imm(10, 22, off);
        a.cmp_imm_w(10, 8);
        a.b_cond(C_NE, plain_h);
        a.ldr_imm(10, 22, off + 8);
        a.add_imm(r, 10, rcv);
        a.ldrb_imm(12, r, ex);
        let ex_ok = a.new_label();
        a.cmp_imm_w(12, none_tag);
        a.b_cond(C_EQ, ex_ok);
        a.cmp_imm_w(12, arr_tag);
        a.b_cond(C_NE, plain_h);
        a.bind(ex_ok);
        a.ldrb_imm(12, r, plain);
        a.cbz(12, false, plain_h);
    }

    // ---- emission state --------------------------------------------------------------------
    // (chain idx, bail label, vstack snapshot, virgins stored at that point)
    let mut bails: Vec<(usize, usize, Vec<LV>, Vec<u32>)> = Vec::new();
    let mut vstack: Vec<LV> = Vec::new();
    let pinned = |x: u32| {
        plan.elem_retain.iter().any(|&(_, p)| p == x)
            || plan.conv_retain.iter().any(|&(_, p)| p == x)
    };
    let mut free_i: Vec<u32> = [1u32, 0, 8, 7, 6, 5, 4, 3, 2]
        .into_iter()
        .filter(|x| !plan.slots.iter().any(|s| s.res == SlotRes::I(*x)) && !pinned(*x))
        .collect();
    let mut free_d: Vec<u32> = (16..24).rev().collect();

    macro_rules! emit_pass {
        ($range:expr, $exit:expr, $base_virgins:expr) => {{
            let mut stores_seen: Vec<u32> = $base_virgins;
            for idx in $range {
                let (ref cop, _) = plan.chain[idx];
                let bail = a.new_label();
                #[allow(unused_assignments)]
                let mut used = false;
                let snap = vstack.clone();
                let seen_snap = stores_seen.clone();
                // Operand registers freed by this op return to the pools only once the op has
                // emitted its last guard — a bail spills the pre-op snapshot, so no operand
                // register may be reused (and clobbered) while a guard can still fire.
                let mut dead: Vec<LV> = Vec::new();
                macro_rules! guard {
                    () => {{
                        #[allow(unused_assignments)]
                        {
                            used = true;
                        }
                        bail
                    }};
                }
                // Convert helpers ------------------------------------------------------------
                macro_rules! to_w {
                    // Value into a w-usable scratch gpr; returns the register number.
                    ($v:expr, $scr:expr) => {{
                        match $v {
                            LV::I(x, _) => x,
                            LV::K(bits) => {
                                let iv = f64::from_bits(bits) as i64;
                                a.mov_imm64($scr, iv as u64);
                                $scr
                            }
                            LV::D(d, iv) => {
                                a.fcvtzs_x_d($scr, d);
                                if !iv {
                                    a.scvtf_d_x(0, $scr);
                                    a.frintz(1, d);
                                    a.fcmp(0, 1);
                                    a.b_cond(C_NE, guard!());
                                    a.cmn_imm_x($scr, 1);
                                    a.b_cond(C_VS, guard!());
                                }
                                $scr
                            }
                        }
                    }};
                }
                macro_rules! free_v {
                    ($v:expr) => {
                        dead.push($v)
                    };
                }
                macro_rules! to_d {
                    // Value into a d-register; the original register is deferred-freed, so the
                    // caller owns the result only if the source was already D.
                    ($v:expr) => {{
                        match $v {
                            LV::D(d, _) => d,
                            LV::I(x, _) => {
                                let d = free_d.pop().expect("loop d pool");
                                a.scvtf_d_x(d, x);
                                dead.push(LV::I(x, false));
                                d
                            }
                            LV::K(bits) => {
                                let d = free_d.pop().expect("loop d pool");
                                a.mov_imm64(9, bits);
                                a.fmov_d_x(d, 9);
                                d
                            }
                        }
                    }};
                }
                macro_rules! key_to_x9 {
                    ($v:expr) => {
                        match $v {
                            LV::I(x, neg) => {
                                if neg {
                                    a.cmp_imm_x(x, 0);
                                    a.b_cond(11, guard!()); // LT
                                }
                                a.mov(9, x);
                                dead.push(LV::I(x, false));
                            }
                            LV::K(bits) => {
                                let f = f64::from_bits(bits);
                                if f.fract() == 0.0 && (0.0..2147483648.0).contains(&f) {
                                    a.mov_imm64(9, f as u64);
                                } else {
                                    a.mov_imm64(9, bits);
                                    a.fmov_d_x(0, 9);
                                    a.fcvtzu_w_d(9, 0);
                                    a.ucvtf_d_w(1, 9);
                                    a.fcmp(0, 1);
                                    a.b_cond(C_NE, guard!());
                                }
                            }
                            LV::D(d, _) => {
                                a.fcvtzu_w_d(9, d);
                                a.ucvtf_d_w(0, 9);
                                a.fcmp(d, 0);
                                a.b_cond(C_NE, guard!());
                                dead.push(LV::D(d, false));
                            }
                        }
                    };
                }
                // Element lookup: key index in x9, receiver base in `r` → entry pointer in x15.
                macro_rules! elem_entry {
                    ($r:expr) => {{
                        a.ldr_imm(12, $r, ell);
                        a.cmp_reg_x(9, 12);
                        a.b_cond(C_HS, guard!());
                        a.ldr_imm(12, $r, elp);
                        a.add_shifted(12, 12, 9, 2);
                        a.ldr_w_imm(13, 12, 0);
                        a.cmn_imm_w(13, 1);
                        a.b_cond(C_EQ, guard!());
                        a.ldr_imm(15, $r, en);
                        a.movz(9, es as u32, 0);
                        a.madd(15, 13, 9, 15);
                        a.ldrb_imm(9, 15, ea);
                        a.cbnz(9, false, guard!());
                    }};
                }

                match *cop {
                    ChainOp::ConstNum(bits) => vstack.push(LV::K(bits)),
                    ChainOp::Load(off) => {
                        let s = slot(off).expect("planned slot");
                        match s.res {
                            SlotRes::F(dres) => {
                                let dt = free_d.pop().expect("loop d pool");
                                a.fmov_d_d(dt, dres);
                                let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                                vstack.push(LV::D(dt, iv));
                            }
                            SlotRes::I(xres) => {
                                let xt = free_i.pop().expect("loop i pool");
                                a.mov(xt, xres);
                                vstack.push(LV::I(xt, true));
                            }
                            SlotRes::None => {
                                a.ldrb_imm(9, 22, off);
                                a.cmp_imm_w(9, 4);
                                a.b_cond(C_NE, guard!());
                                let dt = free_d.pop().expect("loop d pool");
                                a.ldr_d_imm(dt, 22, off + 8);
                                let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                                vstack.push(LV::D(dt, iv));
                            }
                        }
                    }
                    ChainOp::Update(off, kind) => {
                        let s = slot(off).expect("planned slot");
                        let dec = matches!(
                            kind,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
                        );
                        match s.res {
                            SlotRes::I(xres) => {
                                // The entry guard proved exact i32; a flag-setting w-form ±1
                                // keeps it (V = left i32 = bail), far from f64's 2^53 edge.
                                if dec {
                                    a.subs_imm_w(9, xres, 1);
                                } else {
                                    a.adds_imm_w(9, xres, 1);
                                }
                                a.b_cond(C_VS, guard!());
                                a.sxtw(9, 9);
                                match kind {
                                    UpdKind::PostInc | UpdKind::PostDec => {
                                        let xt = free_i.pop().expect("loop i pool");
                                        a.mov(xt, xres);
                                        a.mov(xres, 9);
                                        vstack.push(LV::I(xt, true));
                                    }
                                    UpdKind::PreInc | UpdKind::PreDec => {
                                        a.mov(xres, 9);
                                        let xt = free_i.pop().expect("loop i pool");
                                        a.mov(xt, xres);
                                        vstack.push(LV::I(xt, true));
                                    }
                                    _ => a.mov(xres, 9),
                                }
                            }
                            SlotRes::F(dres) => {
                                let f = if dec { 1 } else { 0 };
                                a.fmov_one(0);
                                match kind {
                                    UpdKind::PostInc | UpdKind::PostDec => {
                                        let dt = free_d.pop().expect("loop d pool");
                                        a.fmov_d_d(dt, dres);
                                        a.f_arith(f, dres, dres, 0);
                                        vstack.push(LV::D(dt, false));
                                    }
                                    UpdKind::PreInc | UpdKind::PreDec => {
                                        a.f_arith(f, dres, dres, 0);
                                        let dt = free_d.pop().expect("loop d pool");
                                        a.fmov_d_d(dt, dres);
                                        vstack.push(LV::D(dt, false));
                                    }
                                    _ => a.f_arith(f, dres, dres, 0),
                                }
                            }
                            SlotRes::None => {
                                a.ldrb_imm(9, 22, off);
                                a.cmp_imm_w(9, 4);
                                a.b_cond(C_NE, guard!());
                                let f = if dec { 1 } else { 0 };
                                a.ldr_d_imm(0, 22, off + 8);
                                a.fmov_one(1);
                                a.f_arith(f, 1, 0, 1);
                                a.str_d_imm(1, 22, off + 8);
                                match kind {
                                    UpdKind::PostInc | UpdKind::PostDec => {
                                        let dt = free_d.pop().expect("loop d pool");
                                        a.fmov_d_d(dt, 0);
                                        vstack.push(LV::D(dt, false));
                                    }
                                    UpdKind::PreInc | UpdKind::PreDec => {
                                        let dt = free_d.pop().expect("loop d pool");
                                        a.fmov_d_d(dt, 1);
                                        vstack.push(LV::D(dt, false));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    ChainOp::GetElem(xoff) => {
                        let key = vstack.pop().expect("loop vstack");
                        // A read the planner proved identical to an earlier one (same receiver,
                        // same key value, no element write between) copies the pinned result —
                        // its guards already passed this iteration.
                        if let Some(&(_, ridx)) =
                            plan.elem_reuse.iter().find(|&&(d, _)| d == idx)
                        {
                            let pin = plan
                                .elem_retain
                                .iter()
                                .find(|&&(i, _)| i == ridx)
                                .expect("planned retain")
                                .1;
                            free_v!(key);
                            if matches!(plan.kinds[idx], PushKind::I { .. }) {
                                let xt = free_i.pop().expect("loop i pool");
                                a.mov(xt, pin);
                                vstack.push(LV::I(xt, true));
                            } else {
                                let dt = free_d.pop().expect("loop d pool");
                                a.fmov_d_d(dt, pin);
                                vstack.push(LV::D(dt, false));
                            }
                        } else {
                            key_to_x9!(key);
                            let r = rcv_reg(xoff).expect("planned receiver");
                            elem_entry!(r);
                            a.ldrb_imm(9, 15, ev as u32);
                            a.cmp_imm_w(9, 4);
                            a.b_cond(C_NE, guard!());
                            let pin =
                                plan.elem_retain.iter().find(|&&(i, _)| i == idx).map(|p| p.1);
                            if matches!(plan.kinds[idx], PushKind::I { .. }) {
                                // w-form: the exactness compare-back also proves i32 (the
                                // planner's range analysis relies on that bound).
                                a.ldur_d(0, 15, ev + 8);
                                let xt = free_i.pop().expect("loop i pool");
                                a.fcvtzs_w_d(xt, 0);
                                a.scvtf_d_w(1, xt);
                                a.fcmp(1, 0);
                                a.b_cond(C_NE, guard!());
                                a.sxtw(xt, xt);
                                if let Some(p) = pin {
                                    a.mov(p, xt);
                                }
                                vstack.push(LV::I(xt, true));
                            } else {
                                let dt = free_d.pop().expect("loop d pool");
                                a.ldur_d(dt, 15, ev + 8);
                                if let Some(p) = pin {
                                    a.fmov_d_d(p, dt);
                                }
                                vstack.push(LV::D(dt, false));
                            }
                        }
                    }
                    ChainOp::SetElem(xoff, keep) => {
                        let val = vstack.pop().expect("loop vstack");
                        let key = vstack.pop().expect("loop vstack");
                        // Stage the value into d2 before the key conversion (d0/d1 scratch).
                        match val {
                            LV::D(d, _) => a.fmov_d_d(2, d),
                            LV::I(x, _) => a.scvtf_d_x(2, x),
                            LV::K(bits) => {
                                a.mov_imm64(9, bits);
                                a.fmov_d_x(2, 9);
                            }
                        }
                        key_to_x9!(key);
                        let r = rcv_reg(xoff).expect("planned receiver");
                        elem_entry!(r);
                        a.ldrb_imm(9, 15, ew);
                        a.cbz(9, false, guard!());
                        a.ldrb_imm(14, 15, ev as u32);
                        a.cmp_imm_w(14, 5);
                        a.b_cond(C_EQ, guard!());
                        let old_plain = a.new_label();
                        a.cmp_imm_w(14, 6);
                        a.b_cond(C_LO, old_plain);
                        a.ldur(12, 15, ev + 8);
                        a.ldur(13, 12, strong);
                        a.cmp_imm_x(13, 1);
                        a.b_cond(C_LS, guard!());
                        a.bind(old_plain);
                        a.movz(9, 4, 0);
                        a.stur(9, 15, ev);
                        a.stur_d(2, 15, ev + 8);
                        a.stur(31, 15, ev + 16);
                        let no_dec = a.new_label();
                        a.cmp_imm_w(14, 6);
                        a.b_cond(C_LO, no_dec);
                        a.ldur(13, 12, strong);
                        a.sub_imm(13, 13, 1);
                        a.stur(13, 12, strong);
                        a.bind(no_dec);
                        if keep {
                            vstack.push(val);
                        } else {
                            free_v!(val);
                        }
                    }
                    ChainOp::Arith(f) => {
                        let b = vstack.pop().expect("loop vstack");
                        let a_ = vstack.pop().expect("loop vstack");
                        if let PushKind::I { neg } = plan.kinds[idx] {
                            // Range-proven exact integer arithmetic: no guards needed.
                            let to_x = |a: &mut asm::Asm, v: LV, scr: u32| match v {
                                LV::I(x, _) => x,
                                LV::K(bits) => {
                                    a.mov_imm64(scr, f64::from_bits(bits) as i64 as u64);
                                    scr
                                }
                                // Planner-proven integral: exact without a guard.
                                LV::D(d, _) => {
                                    a.fcvtzs_x_d(scr, d);
                                    scr
                                }
                            };
                            let xb = to_x(a, b, 10);
                            let xa = to_x(a, a_, 9);
                            let xt = free_i.pop().expect("loop i pool");
                            match f {
                                0 => a.add_shifted(xt, xa, xb, 0),
                                1 => a.sub_reg(xt, xa, xb),
                                _ => a.madd(xt, xa, xb, 31),
                            }
                            free_v!(a_);
                            free_v!(b);
                            vstack.push(LV::I(xt, neg));
                        } else {
                            let db = to_d!(b);
                            let da = to_d!(a_);
                            a.f_arith(f, da, da, db);
                            dead.push(LV::D(db, false));
                            let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                            vstack.push(LV::D(da, iv));
                        }
                    }
                    ChainOp::Bit(code) => {
                        let b = vstack.pop().expect("loop vstack");
                        let a_ = vstack.pop().expect("loop vstack");
                        let neg = matches!(plan.kinds[idx], PushKind::I { neg: true });
                        // A guarded ToInt32 the planner proved repeats an earlier one reuses the
                        // pinned result; the first instance converts into its pin.
                        macro_rules! conv {
                            ($v:expr, $side:expr, $scr:expr) => {{
                                let reuse = plan
                                    .conv_reuse
                                    .iter()
                                    .find(|&&((i, s), _)| i == idx && s == $side)
                                    .map(|p| p.1);
                                match (reuse, $v) {
                                    // The operand register is untouched; the arm's free_v!
                                    // releases it at op end like any other operand.
                                    (Some(pin), LV::D(..)) => pin,
                                    _ => {
                                        let scr = plan
                                            .conv_retain
                                            .iter()
                                            .find(|&&((i, s), _)| i == idx && s == $side)
                                            .map(|p| p.1)
                                            .unwrap_or($scr);
                                        to_w!($v, scr)
                                    }
                                }
                            }};
                        }
                        // Immediate forms when the rhs is a suitable constant.
                        let imm = match b {
                            LV::K(bits) => {
                                let f = f64::from_bits(bits);
                                if f.fract() == 0.0 && (0.0..4294967296.0).contains(&f) {
                                    Some(f as u32)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        let enc = imm.and_then(|m| match code {
                            0..=2 => asm::logical_imm_w(m),
                            _ => Some(m & 31),
                        });
                        let xt;
                        if let Some(field) = enc {
                            let wa = conv!(a_, 0, 9);
                            xt = free_i.pop().expect("loop i pool");
                            match code {
                                0 | 1 | 2 => a.logic_imm_w(code, xt, wa, field),
                                3 => a.lsl_imm_w(xt, wa, field),
                                4 => a.lsr_imm_w(xt, wa, field),
                                _ => a.asr_imm_w(xt, wa, field),
                            }
                            free_v!(a_);
                        } else {
                            let wb = conv!(b, 1, 10);
                            let wa = conv!(a_, 0, 9);
                            xt = free_i.pop().expect("loop i pool");
                            match code {
                                0 => a.logic_w(0, xt, wa, wb),
                                1 => a.logic_w(1, xt, wa, wb),
                                2 => a.logic_w(2, xt, wa, wb),
                                3 => a.shift_w(0, xt, wa, wb),
                                4 => a.shift_w(1, xt, wa, wb),
                                _ => a.shift_w(2, xt, wa, wb),
                            }
                            free_v!(a_);
                            free_v!(b);
                        }
                        if neg {
                            a.sxtw(xt, xt);
                        }
                        vstack.push(LV::I(xt, neg));
                    }
                    ChainOp::Neg => {
                        let v = vstack.pop().expect("loop vstack");
                        let d = to_d!(v);
                        a.fneg(d, d);
                        let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                        vstack.push(LV::D(d, iv));
                    }
                    ChainOp::Store(off) => {
                        let v = vstack.pop().expect("loop vstack");
                        let s = slot(off).expect("planned slot");
                        match s.res {
                            SlotRes::F(dres) => {
                                match v {
                                    LV::D(d, _) => {
                                        a.fmov_d_d(dres, d);
                                        dead.push(LV::D(d, false));
                                    }
                                    LV::I(x, _) => {
                                        a.scvtf_d_x(dres, x);
                                        dead.push(LV::I(x, false));
                                    }
                                    LV::K(bits) => {
                                        a.mov_imm64(9, bits);
                                        a.fmov_d_x(dres, 9);
                                    }
                                }
                            }
                            SlotRes::I(xres) => match v {
                                LV::I(x, _) => {
                                    a.mov(xres, x);
                                    dead.push(LV::I(x, false));
                                }
                                LV::K(bits) => {
                                    let f = f64::from_bits(bits);
                                    a.mov_imm64(xres, f as i64 as u64);
                                }
                                LV::D(..) => unreachable!("planner demotes float-stored I slots"),
                            },
                            SlotRes::None => {
                                let dv = to_d!(v);
                                a.ldrb_imm(9, 22, off);
                                a.cmp_imm_w(9, 5);
                                a.b_cond(C_EQ, guard!());
                                let st_plain = a.new_label();
                                a.cmp_imm_w(9, 6);
                                a.b_cond(C_LO, st_plain);
                                a.ldr_imm(10, 22, off + 8);
                                a.ldur(11, 10, strong);
                                a.cmp_imm_x(11, 1);
                                a.b_cond(C_LS, guard!());
                                a.sub_imm(11, 11, 1);
                                a.stur(11, 10, strong);
                                a.bind(st_plain);
                                a.movz(9, 4, 0);
                                a.str_imm(9, 22, off);
                                a.str_d_imm(dv, 22, off + 8);
                                a.str_imm(31, 22, off + 16);
                                dead.push(LV::D(dv, false));
                            }
                        }
                        if s.virgin && !stores_seen.contains(&off) {
                            stores_seen.push(off);
                        }
                    }
                    ChainOp::Pop => {
                        let v = vstack.pop().expect("loop vstack");
                        free_v!(v);
                    }
                    ChainOp::Dup => {
                        let v = *vstack.last().expect("loop vstack");
                        match v {
                            LV::K(bits) => vstack.push(LV::K(bits)),
                            LV::I(x, neg) => {
                                let xt = free_i.pop().expect("loop i pool");
                                a.mov(xt, x);
                                vstack.push(LV::I(xt, neg));
                            }
                            LV::D(d, iv) => {
                                let dt = free_d.pop().expect("loop d pool");
                                a.fmov_d_d(dt, d);
                                vstack.push(LV::D(dt, iv));
                            }
                        }
                    }
                    ChainOp::KeyNop => {}
                    ChainOp::CmpBranch(neg, _) => {
                        let b = vstack.pop().expect("loop vstack");
                        let a_ = vstack.pop().expect("loop vstack");
                        let k_imm12 = |v: LV| match v {
                            LV::K(bits) => {
                                let f = f64::from_bits(bits);
                                if f.fract() == 0.0 && (0.0..4096.0).contains(&f) {
                                    Some(f as u32)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        let int_neg = match neg {
                            5 => 10, // !(a<b) → GE
                            8 => 12, // !(a<=b) → GT
                            n => n,  // LE/LT/NE/EQ hold for signed ints
                        };
                        match (a_, b) {
                            (LV::I(xa, _), LV::I(xb, _)) => {
                                a.cmp_reg_x(xa, xb);
                                a.b_cond(int_neg, $exit);
                                dead.push(LV::I(xa, false));
                                dead.push(LV::I(xb, false));
                            }
                            (LV::I(xa, _), kb) if k_imm12(kb).is_some() => {
                                a.cmp_imm_x(xa, k_imm12(kb).unwrap());
                                a.b_cond(int_neg, $exit);
                                dead.push(LV::I(xa, false));
                            }
                            (a2, LV::K(bits)) if f64::from_bits(bits) == 0.0 => {
                                let da = to_d!(a2);
                                a.fcmp_zero(da);
                                a.b_cond(neg, $exit);
                                dead.push(LV::D(da, false));
                            }
                            (a2, b2) => {
                                let db = to_d!(b2);
                                let da = to_d!(a2);
                                a.fcmp(da, db);
                                a.b_cond(neg, $exit);
                                dead.push(LV::D(da, false));
                                dead.push(LV::D(db, false));
                            }
                        }
                    }
                    ChainOp::LoadName(_) => unreachable!(),
                }
                if used {
                    bails.push((idx, bail, snap, seen_snap));
                }
                for v in dead {
                    match v {
                        LV::I(x, _) => free_i.push(x),
                        LV::D(d, _) => free_d.push(d),
                        LV::K(_) => {}
                    }
                }
            }
        }};
    }

    // ---- rotated loop ----------------------------------------------------------------------
    emit_pass!(0..plan.cond_len, exit_a, Vec::new());
    a.bind(body_l);
    emit_pass!(plan.cond_len..plan.chain.len(), exit_b, cond_virgins.clone());
    emit_pass!(0..plan.cond_len, exit_b, all_virgins.clone());
    a.b(body_l);

    // ---- exits and bails -------------------------------------------------------------------
    let emit_flush = |a: &mut asm::Asm, virgins: &[u32]| {
        for s in &plan.slots {
            if !s.stored {
                continue;
            }
            if s.virgin && !virgins.contains(&s.off) {
                continue;
            }
            let d = match s.res {
                SlotRes::F(d) => d,
                SlotRes::I(x) => {
                    a.scvtf_d_x(0, x);
                    0
                }
                SlotRes::None => continue, // stores wrote through
            };
            if s.virgin {
                a.movz(9, 4, 0);
                a.str_imm(9, 22, s.off);
                a.str_d_imm(d, 22, s.off + 8);
                a.str_imm(31, 22, s.off + 16);
            } else {
                a.str_d_imm(d, 22, s.off + 8);
            }
        }
    };
    a.bind(exit_a);
    emit_flush(a, &cond_virgins);
    a.b(pc_labels[plan.exit_pc]);
    a.bind(exit_b);
    emit_flush(a, &all_virgins);
    a.b(pc_labels[plan.exit_pc]);

    for (idx, label, snap, seen) in bails {
        a.bind(label);
        for v in &snap {
            match *v {
                LV::K(bits) => {
                    a.mov_imm64(9, bits);
                    a.movz(10, 4, 0);
                    a.stur(10, 20, 0);
                    a.stur(9, 20, 8);
                }
                LV::I(x, _) => {
                    a.scvtf_d_x(0, x);
                    a.movz(9, 4, 0);
                    a.stur(9, 20, 0);
                    a.stur_d(0, 20, 8);
                }
                LV::D(d, _) => {
                    a.movz(9, 4, 0);
                    a.stur(9, 20, 0);
                    a.stur_d(d, 20, 8);
                }
            }
            a.stur(31, 20, 16);
            a.add_imm(20, 20, 24);
        }
        emit_flush(a, &seen);
        let pc = plan.chain[idx].1;
        if pc == plan.head {
            a.b(plain_h);
        } else {
            a.b(pc_labels[pc]);
        }
    }
    plain_h
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
            Op::Return | Op::ReturnUndef | Op::Throw | Op::IterAbortL(_) => {}
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

    let stack_base = stack.as_mut_ptr();
    let env_raw = Rc::as_ptr(&env) as *const u8;
    let mut ctx = JitCtx {
        helpers: i.jit_helpers.as_ptr(),
        stack_base,
        final_sp: stack_base,
        env_raw,
        this_raw: std::ptr::null(),
        global_body: if code.needs_global {
            let b = i.global.borrow();
            &*b as *const crate::value::Object as *const u8
        } else {
            std::ptr::null()
        },
        interp: i as *mut Interp,
        chunk: Rc::as_ptr(chunk),
        env_ref: &env as *const Env,
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
    ctx.this_raw = &ctx.this_val as *const Value;
    let entry: extern "C" fn(*mut JitCtx) -> u64 = unsafe { std::mem::transmute(code.mem) };
    let ok = entry(&mut ctx);
    drop(env); // the env handle must outlive the run (ctx.env_ref aliases it)
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

/// The per-frame buffer size (in `Value`s) of [`Interp::frame_pool`]: slots + operand stack of a
/// JIT fast-call frame carve one fixed raw buffer, so frame setup is a freelist pop + pointer
/// math instead of `Vec` bookkeeping. Frames that need more fall back to the pooled-`Vec` path.
pub(crate) const FRAME_BUF: usize = 256;

/// [`run`] for the JIT→JIT fast call: takes ownership of `argc` argument `Value`s at `args`
/// (moved off the caller's operand stack — the caller must NOT drop them), seeding parameter
/// slots by move instead of clone and dropping any surplus. Only for chunks with no activation
/// environment (`Chunk::jit_no_activation`), so the arguments have exactly one consumer.
/// `env` is borrowed raw: the caller keeps the aliased handle alive across the run.
///
/// # Safety
/// `args..args+argc` must be initialized `Value`s the caller relinquishes entirely; `*env` must
/// outlive the run.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub(crate) unsafe fn run_moved(
    i: &mut Interp,
    chunk: &Rc<Chunk>,
    code: &JitCode,
    env: *const Env,
    this_val: Value,
    args: *mut Value,
    argc: usize,
    // `chunk.jit_frame()`, precomputed by the caller (the cached call reads it from its IC).
    (n_params, n_slots): (usize, usize),
) -> Result<Value, Abrupt> {
    let seed = n_params.min(argc);
    // Frame memory: one fixed-size raw buffer from the freelist ([slots | operand stack]);
    // oversized frames use the legacy pooled-Vec pair. The buffer is a plain allocation (not a
    // bump arena), so parked coroutines holding frames on other threads can't be aliased.
    let mut legacy: Option<(Vec<Value>, Vec<Value>)> = None;
    let (slots_ptr, stack_base) = if n_slots + code.max_stack <= FRAME_BUF {
        let buf = i.frame_pool.pop().unwrap_or_else(|| {
            let b: Box<[std::mem::MaybeUninit<Value>]> = Box::new_uninit_slice(FRAME_BUF);
            std::ptr::NonNull::new(Box::into_raw(b) as *mut Value).unwrap()
        });
        (buf.as_ptr(), unsafe { buf.as_ptr().add(n_slots) })
    } else {
        let (mut slots, mut stack) = i.vm_pool.pop().unwrap_or_default();
        slots.reserve(n_slots);
        stack.clear();
        stack.reserve(code.max_stack);
        let p = (slots.as_mut_ptr(), stack.as_mut_ptr());
        legacy = Some((slots, stack));
        p
    };
    unsafe {
        std::ptr::copy_nonoverlapping(args, slots_ptr, seed);
        // Surplus arguments were still moved to us: drop them.
        for k in seed..argc {
            std::ptr::drop_in_place(args.add(k));
        }
        // Initializing a slot to Undefined only needs the tag byte (repr(u8) discriminant 0):
        // no consumer reads a Value's payload behind tag 0, so stale payload bytes are dead.
        for k in seed..n_slots {
            *(slots_ptr.add(k) as *mut u8) = 0;
        }
        for &s in chunk.jit_var_force_resets() {
            let s = s as usize;
            if s < seed {
                std::ptr::drop_in_place(slots_ptr.add(s));
                slots_ptr.add(s).write(Value::Undefined);
            }
        }
    }

    let env_raw = Rc::as_ptr(unsafe { &*env }) as *const u8;
    let mut ctx = JitCtx {
        helpers: i.jit_helpers.as_ptr(),
        stack_base,
        final_sp: stack_base,
        env_raw,
        this_raw: std::ptr::null(),
        global_body: if code.needs_global {
            let b = i.global.borrow();
            &*b as *const crate::value::Object as *const u8
        } else {
            std::ptr::null()
        },
        interp: i as *mut Interp,
        chunk: Rc::as_ptr(chunk),
        env_ref: env,
        this_val,
        slots: slots_ptr,
        inline_ic_safe: &i.inline_ic_safe as *const std::cell::Cell<bool> as *const u8,
        n_slots,
        handlers: Vec::new(),
        code_base: code.mem,
        pc_offsets: code.pc_offsets.as_ptr(),
        error: None,
        ret: Value::Undefined,
    };
    ctx.this_raw = &ctx.this_val as *const Value;
    let entry: extern "C" fn(*mut JitCtx) -> u64 = unsafe { std::mem::transmute(code.mem) };
    let ok = entry(&mut ctx);
    unsafe {
        let mut p = ctx.stack_base;
        while p < ctx.final_sp {
            std::ptr::drop_in_place(p);
            p = p.add(1);
        }
        // Drop the frame's local slots (initialized Values throughout the run). Numeric frames
        // are the common case: a tag peek skips the outlined drop for trivially-copyable tags
        // (Undefined/Empty/Null/Bool/Num — repr(u8) discriminants 0..=4).
        for k in 0..n_slots {
            let p = slots_ptr.add(k);
            if *(p as *const u8) >= 5 {
                std::ptr::drop_in_place(p);
            }
        }
    }
    match legacy {
        None => {
            let buf = unsafe { std::ptr::NonNull::new_unchecked(slots_ptr) };
            if i.frame_pool.len() < 64 {
                i.frame_pool.push(buf);
            } else {
                unsafe {
                    drop(Box::from_raw(std::slice::from_raw_parts_mut(
                        slots_ptr as *mut std::mem::MaybeUninit<Value>,
                        FRAME_BUF,
                    )));
                }
            }
        }
        Some((mut slots, stack)) => {
            // The values were dropped above; the Vec must not double-drop them.
            unsafe { slots.set_len(0) };
            if i.vm_pool.len() < 64 {
                i.vm_pool.push((slots, stack));
            }
        }
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

/// See the aarch64-macos definition; without machine code the fast call never commits.
#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
pub(crate) unsafe fn run_moved(
    _i: &mut Interp,
    _chunk: &Rc<Chunk>,
    _code: &JitCode,
    _env: *const Env,
    _this_val: Value,
    _args: *mut Value,
    _argc: usize,
    _frame: (usize, usize),
) -> Result<Value, Abrupt> {
    unreachable!("jit code cannot exist on this platform")
}
