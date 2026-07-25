//! Native template JIT: the third execution tier.
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
//! The mature backend emits ARM64 on desktop operating systems. A correctness-first x86-64
//! backend emits native control flow on Intel macOS, Linux, and Windows while its hot inline
//! templates are filled in. Other targets retain the bytecode VM.

#![cfg_attr(
    not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows"))),
    allow(dead_code)
)]

use std::rc::Rc;

use crate::bytecode::Chunk;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
use crate::bytecode::UpdKind;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

/// ARM64's generated templates use owned 8-byte NaN-boxed local slots. The x64 backend keeps
/// the established wide `Value` ABI until its load/store templates are migrated as a unit.
const PACKED_LOCAL_SLOTS: bool = false;

// ---------------------------------------------------------------------------------------------
// Executable memory (platform W^X policy)
// ---------------------------------------------------------------------------------------------

#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_os = "macos"
))]
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
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn pthread_jit_write_protect_np(enabled: i32);
        fn sys_icache_invalidate(start: *mut u8, len: usize);
    }
    const PROT_RWX: i32 = 0x1 | 0x2 | 0x4;
    const MAP_PRIVATE_ANON_JIT: i32 = 0x0002 | 0x1000 | 0x0800;

    pub unsafe fn alloc_exec(src: *const u8, len: usize) -> *mut u8 {
        let mem = mmap(
            std::ptr::null_mut(),
            len,
            PROT_RWX,
            MAP_PRIVATE_ANON_JIT,
            -1,
            0,
        );
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        pthread_jit_write_protect_np(0);
        std::ptr::copy_nonoverlapping(src, mem, len);
        pthread_jit_write_protect_np(1);
        sys_icache_invalidate(mem, len);
        mem
    }

    pub unsafe fn free_exec(mem: *mut u8, len: usize) {
        munmap(mem, len);
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod sys {
    extern "C" {
        fn mmap(
            addr: *mut u8,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;

    pub unsafe fn alloc_exec(src: *const u8, len: usize) -> *mut u8 {
        let mem = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE_ANON,
            -1,
            0,
        );
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(src, mem, len);
        if mprotect(mem, len, PROT_READ | PROT_EXEC) != 0 {
            munmap(mem, len);
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn free_exec(mem: *mut u8, len: usize) {
        munmap(mem, len);
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
mod sys {
    use core::arch::asm;

    extern "C" {
        fn mmap(
            addr: *mut u8,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;

    unsafe fn flush_icache(start: *mut u8, len: usize) {
        let ctr: usize;
        asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nostack, preserves_flags));
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        let end = start as usize + len;
        let mut p = (start as usize) & !(dline - 1);
        while p < end {
            asm!("dc cvau, {p}", p = in(reg) p, options(nostack, preserves_flags));
            p += dline;
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        p = (start as usize) & !(iline - 1);
        while p < end {
            asm!("ic ivau, {p}", p = in(reg) p, options(nostack, preserves_flags));
            p += iline;
        }
        asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }

    pub unsafe fn alloc_exec(src: *const u8, len: usize) -> *mut u8 {
        let mem = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE_ANON,
            -1,
            0,
        );
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(src, mem, len);
        flush_icache(mem, len);
        if mprotect(mem, len, PROT_READ | PROT_EXEC) != 0 {
            munmap(mem, len);
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn free_exec(mem: *mut u8, len: usize) {
        munmap(mem, len);
    }
}

#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_os = "windows"
))]
mod sys {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, len: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualProtect(addr: *mut u8, len: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut u8, len: usize, kind: u32) -> i32;
        fn FlushInstructionCache(process: *mut u8, addr: *const u8, len: usize) -> i32;
        fn GetCurrentProcess() -> *mut u8;
    }
    const MEM_COMMIT_RESERVE: u32 = 0x1000 | 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;

    pub unsafe fn alloc_exec(src: *const u8, len: usize) -> *mut u8 {
        let mem = VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
        );
        if mem.is_null() {
            return mem;
        }
        std::ptr::copy_nonoverlapping(src, mem, len);
        let mut old = 0;
        if VirtualProtect(mem, len, PAGE_EXECUTE_READ, &mut old) == 0
            || FlushInstructionCache(GetCurrentProcess(), mem, len) == 0
        {
            VirtualFree(mem, 0, MEM_RELEASE);
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn free_exec(mem: *mut u8, _len: usize) {
        VirtualFree(mem, 0, MEM_RELEASE);
    }
}

/// A finished JIT compilation: executable code plus the pc→code-offset table the unwinder uses
/// to land on catch handlers.
#[repr(C)]
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

impl JitCode {
    /// The machine-code entry address (for CallIc fills — the direct-call sequence branches
    /// to it through the swapped ctx).
    pub(crate) fn mem_ptr(&self) -> *const u8 {
        self.mem
    }
    /// The pc→code-offset table's data pointer (same purpose).
    pub(crate) fn pc_offsets_ptr(&self) -> *const u32 {
        self.pc_offsets.as_ptr()
    }
}

impl Drop for JitCode {
    fn drop(&mut self) {
        #[cfg(any(
            all(
                target_arch = "aarch64",
                any(target_os = "macos", target_os = "linux", target_os = "windows")
            ),
            all(
                target_arch = "x86_64",
                any(target_os = "macos", target_os = "linux", target_os = "windows")
            )
        ))]
        unsafe {
            sys::free_exec(self.mem, self.len);
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
    /// copies the 16-byte Value and bumps its refcount from machine code.
    pub this_raw: *const Value,
    /// [56] The current realm's global `Object` (through the Rc and RefCell): the LoadName
    /// templates' global-mode path validates the cached shape/slot against it.
    pub global_body: *const u8,
    /// [64] `Rc::as_ptr` of the active realm's global scope: the call template's inline probe
    /// compares it against the CallIc's fill-time `global_env` (the same-realm proof).
    pub genv: usize,
    // ---- Rust-only fields ----
    pub interp: *mut Interp,
    pub chunk: *const Chunk,
    pub this_val: Value,
    pub n_slots: usize,
    /// ARM64 generated code keeps local slots as owned NaN-boxed words. Rust helpers expand them
    /// in place and restore them before returning to generated code. x64 remains wide until its
    /// templates migrate.
    pub slots_packed: bool,
    /// Active `try` regions: (catch pc, operand-stack depth to unwind to).
    pub handlers: Vec<(u32, usize)>,
    /// The handler-stack watermark of THIS activation: `jit_unwind` propagates out (instead of
    /// popping) once `handlers.len()` reaches it. Always 0 for a `run`/`run_moved` activation
    /// (each owns a fresh Vec); the direct-call sequence shares the caller's ctx — and its
    /// handlers Vec — so it swaps this to the live length for the callee's duration.
    pub handler_floor: usize,
    pub code_base: *const u8,
    pub pc_offsets: *const u32,
    pub error: Option<Abrupt>,
    pub ret: Value,
}

impl JitCtx {
    /// Enter a Rust helper that expects ordinary 16-byte `Value` slots. Packed slots expand
    /// backward inside their already-reserved wide slot region, transferring ownership.
    pub(crate) unsafe fn unpack_slots(&mut self) {
        if self.slots_packed {
            unsafe { crate::value::PackedValue::unpack_in_place(self.slots, self.n_slots) };
            self.slots_packed = false;
        }
    }

    /// Return from a Rust helper to generated code. Wide slots compact forward in place.
    pub(crate) unsafe fn pack_slots(&mut self) {
        if !self.slots_packed {
            unsafe { crate::value::PackedValue::pack_in_place(self.slots, self.n_slots) };
            self.slots_packed = true;
        }
    }
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
        crate::bytecode::jit_call_hit as *const () as usize,
        crate::bytecode::jit_direct_finish as *const () as usize,
        crate::bytecode::jit_drop_at as *const () as usize,
        crate::bytecode::jit_make_object as *const () as usize,
        crate::bytecode::jit_set_prop as *const () as usize,
        crate::bytecode::jit_get_prop as *const () as usize,
        crate::bytecode::jit_intrinsic as *const () as usize,
        crate::bytecode::jit_new as *const () as usize,
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
/// The call template's inline way-1 probe hit: skips the helper-side decode and probe loop.
pub const H_CALL_HIT: usize = 7;
/// Teardown for a direct (shared-ctx) call: drops, frame-pool return, FnFrame pop, tail drain.
pub const H_DIRECT_FINISH: usize = 8;
/// Drop the single `Value` at `sp` (the direct-call sequence's rare last-reference callee).
pub const H_DROP_AT: usize = 9;
/// Dedicated `Op::MakeObject` entry: template clone + stack-direct value writes, no op decode.
pub const H_MAKE_OBJECT: usize = 10;
/// Dedicated property-store entry (`SetProp`/`SetPropDrop`/`SetPropThisDrop`/`SetPropLocalDrop`
/// misses): straight into `set_prop_ic`, no generic op decode.
pub const H_SET_PROP: usize = 11;
/// Dedicated property-read entry (`GetProp`/`GetPropThis`/`GetPropLocal`/`GetMethod` misses):
/// straight into `get_prop_ic`.
pub const H_GET_PROP: usize = 12;
/// Specialized native calls (`String#slice`, `Object.hasOwn`) after the call IC identity probe.
pub const H_INTRINSIC: usize = 13;
/// Dedicated `Op::New` entry: constructor-cache probe and dispatch without generic op decode.
pub const H_NEW: usize = 14;

/// One-time semantic guard for the numeric packed-array region. A keyless Empty slot is still a
/// missing property, so filling it may be intercepted by an indexed setter on Array.prototype.
/// Reuse the interpreter's live element-protector proof before returning a borrowed Vec header;
/// generated code performs only drop-free Empty/Number overwrites until it leaves the region.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
unsafe extern "C" fn jit_prepare_numeric_packed_array(
    ctx: *mut JitCtx,
    raw: *const std::cell::RefCell<crate::value::Object>,
    len: usize,
) -> *mut u8 {
    if ctx.is_null() || raw.is_null() || len == 0 || len > 8 {
        return std::ptr::null_mut();
    }
    let ctx = unsafe { &mut *ctx };
    let interp = unsafe { &mut *ctx.interp };
    // `raw` is Rc::as_ptr. The frame owns the live Gc throughout this call and the generated
    // region, so borrow an Rc view without changing its strong count.
    let obj = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(raw) });
    {
        let b = obj.borrow();
        if !matches!(&b.exotic, crate::value::Exotic::Array)
            || !b.ic_plain.get()
            || !b.extensible
        {
            return std::ptr::null_mut();
        }
    }
    if interp.array_length(&obj) != len || !interp.array_append_unshadowed(&obj) {
        return std::ptr::null_mut();
    }
    let slots = obj
        .borrow_mut()
        .props
        .jit_packed_numeric_slots(len)
        .map_or(std::ptr::null_mut(), |p| p.cast());
    slots
}

/// Materialize the two live locals at the scheduler active-path exit. The region has already
/// guarded both source objects and will perform no further fallible checks. Doing the replacement
/// in Rust preserves exact destruction for stale compiler temporaries that may be their Rc's last
/// owner; attempting that rare destructor path in generated code would either leak or undercount.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
unsafe extern "C" fn jit_scheduler_materialize(
    tcb_slot: *mut crate::value::Value,
    packet_slot: *mut crate::value::Value,
    tcb_raw: *const std::cell::RefCell<crate::value::Object>,
    packet_raw: *const std::cell::RefCell<crate::value::Object>,
) {
    debug_assert!(!tcb_slot.is_null() && !packet_slot.is_null() && !tcb_raw.is_null());
    // Both raw pointers are Rc::as_ptr views whose real owners remain live across this call.
    let tcb = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(tcb_raw) });
    let new_tcb = crate::value::Value::Obj(Rc::clone(&*tcb));
    let new_packet = if packet_raw.is_null() {
        crate::value::Value::Null
    } else {
        let packet = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(packet_raw) });
        crate::value::Value::Obj(Rc::clone(&*packet))
    };
    // Construct both new owners before either stale local is released so source/old aliases are
    // harmless and an object graph cannot disappear between the replacements.
    let old_tcb = unsafe { std::ptr::replace(tcb_slot, new_tcb) };
    let old_packet = unsafe { std::ptr::replace(packet_slot, new_packet) };
    drop(old_packet);
    drop(old_tcb);
}

/// Materialize the locals expected by the inlined DeviceTask body after a guarded virtual
/// dispatch. Sources remain owned by the TCB and active-prefix locals; all destination values
/// are constructed before stale compiler temporaries are released, so arbitrary aliasing and
/// last-owner destruction are safe.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
unsafe extern "C" fn jit_scheduler_device_materialize(
    packet_src: *const crate::value::Value,
    packet_dst: *mut crate::value::Value,
    task_dst: *mut crate::value::Value,
    temp_dst: *mut crate::value::Value,
    task_raw: *const std::cell::RefCell<crate::value::Object>,
) {
    debug_assert!(
        !packet_src.is_null()
            && !packet_dst.is_null()
            && !task_dst.is_null()
            && !temp_dst.is_null()
            && !task_raw.is_null()
    );
    let packet = unsafe { (&*packet_src).clone() };
    let task = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(task_raw) });
    let task = crate::value::Value::Obj(Rc::clone(&*task));
    let old_packet = unsafe { std::ptr::replace(packet_dst, packet) };
    let old_task = unsafe { std::ptr::replace(task_dst, task) };
    let old_temp = unsafe {
        std::ptr::replace(temp_dst, crate::value::Value::Undefined)
    };
    drop(old_temp);
    drop(old_task);
    drop(old_packet);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
unsafe extern "C" fn jit_scheduler_trace_fail(stage: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
    if let Some(counter) = COUNTS.get(stage) {
        let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            eprintln!("[jit-scheduler] active guard stage {stage}: {count}");
        }
    }
}
pub const N_HELPERS: usize = 15;

/// ARM64 condition codes used by the inline templates.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_EQ: u32 = 0;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_NE: u32 = 1;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_HS: u32 = 2;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_LO: u32 = 3;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_MI: u32 = 4;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_HI: u32 = 8;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_LS: u32 = 9;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_GE: u32 = 10;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_GT: u32 = 12;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_LE: u32 = 13;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const C_VS: u32 = 6;

/// Condition-helper modes (the `w1` immediate for `H_COND`).
pub const COND_POP_TRUTHY: u32 = 0;
pub const COND_PEEK_TRUTHY: u32 = 1;
#[cfg(all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub const COND_PEEK_NOT_NULLISH: u32 = 2;

// The inline fast paths read Value directly: repr(u8) tag byte at offset 0, payload at
// offset 8, 16 bytes total on 64-bit. Tags 0..=4 (Undefined/Empty/Null/Bool/Num) are trivially
// copyable. Only 64-bit desktop JIT targets depend on this; on wasm32 `Value` is smaller.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
const _: () = assert!(std::mem::size_of::<Value>() == 16);
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
const _: () = assert!(std::mem::align_of::<Value>() == 8);
// The offsets below bake 8-byte pointers into the emitted templates: JIT-platform only (on
// wasm32 pointers are 4 bytes and none of this code exists).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod layout_asserts {
    use super::JitCtx;
    // The call template's inline way-1 probe reads these CallIc fields by fixed offset.
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, callee) == 0);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, global_env) == 32);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, epoch) == 56);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, n_params) == 42);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, n_slots) == 44);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, direct) == 46);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, chunk_raw) == 64);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, code_mem) == 72);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, pc_offs_ptr) == 80);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, native) == 88);
    const _: () = assert!(std::mem::offset_of!(crate::bytecode::CallIc, intrinsic) == 96);
    const _: () = assert!(std::mem::size_of::<std::cell::Cell<crate::bytecode::CallIc>>() == 104);
    const _: () = assert!(std::mem::offset_of!(JitCtx, genv) == 64);
    // 3b reads Interp state from machine code through ctx.interp.
    const _: () = assert!(std::mem::offset_of!(JitCtx, interp) == 72);
    // The asm frame push writes FnFrame fields by fixed offset.
    const _: () = assert!(std::mem::offset_of!(crate::interpreter::FnFrame, fn_ptr) == 0);
    const _: () = assert!(std::mem::offset_of!(crate::interpreter::FnFrame, coro) == 8);
    const _: () = assert!(std::mem::offset_of!(crate::interpreter::FnFrame, strict) == 12);
    const _: () = assert!(std::mem::offset_of!(crate::interpreter::FnFrame, extra) == 16);
    const _: () = assert!(std::mem::size_of::<crate::interpreter::FnFrame>() == 24);
    // The direct-call sequence reads the callee's code/pc_offsets straight from its JitCode.
    const _: () = assert!(std::mem::offset_of!(super::JitCode, mem) == 0);
    const _: () = assert!(std::mem::offset_of!(super::JitCode, pc_offsets) == 16);
}

/// Two-register return for helpers that produce (new sp, flag) — x0/x1 under the C ABI.
#[repr(C)]
pub struct SpFlag {
    pub sp: *mut Value,
    pub flag: u64,
}

// ---------------------------------------------------------------------------------------------
// ARM64 assembler (the ~20 encodings the templates need)
// ---------------------------------------------------------------------------------------------

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
        /// str wt, [xn, #imm] (scaled, imm/4)
        pub fn str_w_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes.is_multiple_of(4) && imm_bytes / 4 < 4096);
            self.emit(0xB900_0000 | ((imm_bytes / 4) << 10) | (rn << 5) | rt);
        }
        /// ldr xt, [xn, xm, lsl #3] (register-offset, scaled)
        pub fn ldr_x_lsl3(&mut self, rt: u32, rn: u32, rm: u32) {
            self.emit(0xF860_7800 | (rm << 16) | (rn << 5) | rt);
        }
        /// ldrb wt, [xn, xm] (register-offset, unscaled)
        pub fn ldrb_reg(&mut self, rt: u32, rn: u32, rm: u32) {
            self.emit(0x3860_6800 | (rm << 16) | (rn << 5) | rt);
        }
        /// ldrh wt, [xn, #imm] (scaled, imm/2)
        pub fn ldrh_imm(&mut self, rt: u32, rn: u32, imm_bytes: u32) {
            debug_assert!(imm_bytes.is_multiple_of(2) && imm_bytes / 2 < 4096);
            self.emit(0x7940_0000 | ((imm_bytes / 2) << 10) | (rn << 5) | rt);
        }
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
            debug_assert!(
                imm_bytes % 8 == 0 && (-512..=504).contains(&imm_bytes),
                "AArch64 STP pre-index offset out of signed imm7 range: {imm_bytes}"
            );
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA980_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// ldp xt1, xt2, [sp], #imm (post-index)
        pub fn ldp_post(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            debug_assert!(
                imm_bytes % 8 == 0 && (-512..=504).contains(&imm_bytes),
                "AArch64 LDP post-index offset out of signed imm7 range: {imm_bytes}"
            );
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA8C0_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// stp xt1, xt2, [sp, #imm] (signed offset form)
        pub fn stp_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            debug_assert!(
                imm_bytes % 8 == 0 && (-512..=504).contains(&imm_bytes),
                "AArch64 STP signed offset out of imm7 range: {imm_bytes}"
            );
            let imm7 = ((imm_bytes / 8) & 0x7f) as u32;
            self.emit(0xA900_0000 | (imm7 << 15) | (rt2 << 10) | (31 << 5) | rt1);
        }
        /// ldp xt1, xt2, [sp, #imm]
        pub fn ldp_off(&mut self, rt1: u32, rt2: u32, imm_bytes: i32) {
            debug_assert!(
                imm_bytes % 8 == 0 && (-512..=504).contains(&imm_bytes),
                "AArch64 LDP signed offset out of imm7 range: {imm_bytes}"
            );
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
        /// bl label (patched; same imm26 shape as B). The callee stub must preserve x19..x22
        /// and, if it calls out itself, spill/reload x30.
        pub fn bl_label(&mut self, label: usize) {
            self.patches.push((self.buf.len(), label, PatchKind::B));
            self.emit(0x9400_0000);
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
        /// fsqrt dd, dn
        pub fn fsqrt(&mut self, rd: u32, rn: u32) {
            self.emit(0x1E61_C000 | (rn << 5) | rd);
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
        /// mvn wd, wm (ORN wd, wzr, wm): bitwise complement of the low 32 bits.
        pub fn mvn_w(&mut self, rd: u32, rm: u32) {
            self.emit(0x2A20_03E0 | (rm << 16) | rd);
        }
        /// and/orr/eor xd, xn, xm — op: 0=and, 1=orr, 2=eor
        pub fn logic_x(&mut self, op: u32, rd: u32, rn: u32, rm: u32) {
            let bits = match op {
                0 => 0x8A00_0000u32,
                1 => 0xAA00_0000,
                _ => 0xCA00_0000,
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
        /// lsl xd, xn, #shift (UBFM alias)
        pub fn lsl_imm(&mut self, rd: u32, rn: u32, shift: u32) {
            debug_assert!(shift < 64);
            let immr = (64 - shift) & 63;
            let imms = 63 - shift;
            self.emit(0xD340_0000 | (immr << 16) | (imms << 10) | (rn << 5) | rd);
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
        /// fmov xd, dn (bit move)
        pub fn fmov_x_d(&mut self, rd: u32, rn: u32) {
            self.emit(0x9E66_0000 | (rn << 5) | rd);
        }
        /// ldr dt, [xn, xm, lsl #3]
        pub fn ldr_d_lsl3(&mut self, rt: u32, rn: u32, rm: u32) {
            self.emit(0xFC60_7800 | (rm << 16) | (rn << 5) | rt);
        }
        /// str dt, [xn, xm, lsl #3]
        pub fn str_d_lsl3(&mut self, rt: u32, rn: u32, rm: u32) {
            self.emit(0xFC20_7800 | (rm << 16) | (rn << 5) | rt);
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
            // Relax imm19 branches that cannot reach after final layout. Invert the local
            // condition over an imm26 B and update every later label/patch for the inserted word.
            // Iteration matters: one insertion can push another branch just over its limit.
            loop {
                let Some(k) = self.patches.iter().position(|(at, label, kind)| {
                    if !matches!(kind, PatchKind::Cb) {
                        return false;
                    }
                    let target = self.labels[*label].expect("unbound jit label");
                    let delta = target as i64 - *at as i64;
                    !(-(1 << 18)..(1 << 18)).contains(&delta)
                }) else {
                    break;
                };
                let (at, label, _) = self.patches[k];
                let insn = self.buf[at];
                self.buf[at] = if insn & 0xff00_0000 == 0x5400_0000 {
                    insn ^ 1 // B.cond: invert the low condition bit
                } else {
                    insn ^ 0x0100_0000 // CBZ <-> CBNZ
                } | (2 << 5); // skip the following B
                self.buf.insert(at + 1, 0x1400_0000);
                for bound in self.labels.iter_mut().flatten() {
                    if *bound > at {
                        *bound += 1;
                    }
                }
                for (patch_at, _, _) in &mut self.patches {
                    if *patch_at > at {
                        *patch_at += 1;
                    }
                }
                self.patches[k] = (at + 1, label, PatchKind::B);
            }
            for (at, label, kind) in std::mem::take(&mut self.patches) {
                let target = self.labels[label].expect("unbound jit label");
                let delta = target as i64 - at as i64; // in instructions
                match kind {
                    PatchKind::B => {
                        assert!(
                            (-(1 << 25)..(1 << 25)).contains(&delta),
                            "JIT imm26 branch out of range: {delta} instructions"
                        );
                        let imm26 = (delta as u32) & 0x03FF_FFFF;
                        self.buf[at] |= imm26;
                    }
                    PatchKind::Cb => {
                        debug_assert!((-(1 << 18)..(1 << 18)).contains(&delta));
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

        #[test]
        fn far_condition_uses_an_unconditional_veneer() {
            let mut a = super::Asm::new();
            let target = a.new_label();
            a.b_cond(super::super::C_EQ, target);
            // Exceed imm19's positive limit (262,143 instructions). A raw conditional branch
            // would wrap into unrelated generated code; the veneer keeps its conditional local.
            for _ in 0..270_000 {
                a.mov(0, 0);
            }
            a.bind(target);
            let code = a.finish();
            assert_eq!((code[0] >> 5) & 0x7ffff, 2); // inverted condition skips the B
            assert_eq!(code[0] & 0xf, super::super::C_NE);
            assert_eq!(code[1] >> 26, 0b000101); // unconditional B (imm26)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------------------------

/// Compile `chunk` to machine code, or `None` when unsupported (non-macOS/ARM64, async bodies,
/// or an op stream whose stack depths don't line up — a compiler bug caught defensively).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn compile(
    chunk: &Chunk,
    layout: &crate::value::JitLayout,
    ilayout: &crate::interpreter::InterpLayout,
) -> Option<JitCode> {
    use crate::bytecode::{Op, UpdKind};

    let ops = chunk.jit_ops();
    if ops.len() > 0xFFFF {
        return None; // op index must fit one movz
    }
    // Async bodies suspend; the VM's coroutine runs them.
    if ops.iter().any(|o| matches!(o, Op::Await)) {
        return None;
    }
    let cfg = crate::jit_ir::Cfg::build(chunk).ok()?;
    let max_stack = cfg.jit_stack_capacity();
    // Debug: `LUMEN_JIT_DUMP=<substr>` prints the op stream of chunks whose leading slot names
    // contain the substring (empty value = all chunks) as they compile.
    if let Ok(pat) = std::env::var("LUMEN_JIT_DUMP") {
        let head: Vec<&str> = chunk
            .jit_slot_names()
            .iter()
            .take(4)
            .map(|s| &**s)
            .collect();
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
    let array_intrinsics_on = std::env::var_os("LUMEN_JIT_NO_ARRAY_INTRINSICS").is_none();
    let function_call_intrinsic_on =
        std::env::var_os("LUMEN_JIT_NO_FUNCTION_CALL_INTRINSIC").is_none();
    // Direct shared-ctx calls, on by default like every other emitter feature (mask bit 20
    // off for debugging). Requires the inline call probe (bit 524288) to emit at all.
    let direct_on = fast & (1 << 20) != 0;
    // Whether the probed layout supports inline refcount bumps/decs (clone/drop of Str/Sym/Obj
    // without a helper call). All strong-count templates gate on this.
    let rc_ok = layout.valid && layout.rc_strong_off < 256;
    let rc_strong = layout.rc_strong_off as i32;
    // Preplanning the one Richards-style scheduler shell lets only that chunk pay for six extra
    // callee-saved registers. x23..x27 retain epoch-stable scheduler/prototype/global facts and
    // x28 is a bounded continuation budget; every other JIT chunk keeps the compact frame.
    let mut fast_scheduler_shell = if fast & (1 << 22) != 0
        && fast & 32768 != 0
        && rc_ok
        && std::env::var_os("LUMEN_JIT_NO_SCHED_FAST_LOOP").is_none()
    {
        ops.iter().enumerate().find_map(|(head, _)| {
            let plan = plan_scheduler_shell(chunk, ops, head, &cfg, layout, fast)?;
            plan.active.as_ref()?;
            Some((head, plan))
        })
    } else {
        None
    };
    let scheduler_frame = fast_scheduler_shell.is_some();
    let scheduler_role_epoch = fast_scheduler_shell
        .as_ref()
        .is_some_and(|(_, plan)| scheduler_role_epoch_enabled(plan));
    let scheduler_graph_epoch = fast_scheduler_shell
        .as_ref()
        .is_some_and(|(_, plan)| scheduler_graph_epoch_enabled(plan, layout));
    let mut a = asm::Asm::new();
    // One label per bytecode pc (branch/catch targets bind as we emit).
    let pc_labels: Vec<usize> = (0..ops.len()).map(|_| a.new_label()).collect();
    let has_active_null_dispatch = fast_scheduler_shell
        .as_ref()
        .and_then(|(_, shell)| shell.active.as_ref())
        .and_then(|active| active.null_dispatch.as_ref())
        .is_some();
    // Forward target after pc59's canonical Device/Handler classifiers. A virtual Active-null
    // miss materializes its snapshot and enters ordinary bytecode here without repeating them.
    let scheduler_plain_dispatch = has_active_null_dispatch.then(|| a.new_label());
    let l_unwind = a.new_label();
    let l_ret_ok = a.new_label();
    let l_ret_throw = a.new_label();
    // The direct-call teardown stub (one per chunk, `bl`-reached; emitted after the epilogues).
    let l_direct_finish = a.new_label();

    // ---- prologue ----
    // Frame: save fp/lr + x19..x22 (x19=ctx, x20=sp, x21=helpers, x22=slots) + d8..d15.
    // The scheduler chunk alone also saves x23..x28 for its bounded internal continuation and
    // reserves four words for the session-local task-role prototype cache. A graph-compatible
    // scheduler additionally owns a fixed 320-byte non-owning cache: 32-byte header, six
    // 48-byte TCB/task records. Its 512-byte frame needs an explicit SP adjustment because the
    // paired post-index form tops out at +504 bytes.
    let frame_size = if scheduler_graph_epoch {
        512
    } else if scheduler_frame {
        192
    } else {
        112
    };
    let dsave = if scheduler_graph_epoch {
        448
    } else if scheduler_frame {
        128
    } else {
        48
    };
    if scheduler_graph_epoch {
        a.sub_imm(31, 31, frame_size as u32);
        a.stp_off(29, 30, 0);
    } else {
        a.stp_pre(29, 30, -frame_size);
    }
    a.stp_off(19, 20, 16);
    a.stp_off(21, 22, 32);
    if scheduler_frame {
        a.stp_off(23, 24, 48);
        a.stp_off(25, 26, 64);
        a.stp_off(27, 28, 80);
        a.movz(28, 0, 0);
    }
    a.stp_d_off(8, 9, dsave);
    a.stp_d_off(10, 11, dsave + 16);
    a.stp_d_off(12, 13, dsave + 32);
    a.stp_d_off(14, 15, dsave + 48);
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
            | Op::InlineGuard(_, t)
            | Op::PushHandler(t) => targeted[*t as usize] = true,
            _ => {}
        }
    }
    let mut scheduler_fast_resume = None;
    let mut scheduler_dispatch_pc = None;

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
        // IdleTask's dominant release arm is a whole-function guarded transaction. Success
        // returns directly; a declined guard lands on the untouched pc0 template so accessors,
        // coercions, partial effects, and the one final hold retain exact bytecode behavior.
        if pc == 0 && rc_ok {
            if let Some(plan) = plan_scheduler_idle_release(chunk, ops, &cfg, layout, fast) {
                let plain_h = emit_scheduler_idle_release_region(
                    &mut a,
                    layout,
                    &plan,
                    l_ret_ok,
                );
                a.bind(plain_h);
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!("[jit-region] head 0: EMITTED scheduler Idle release");
                }
            }
        }
        // Handler's v2-delivery transaction begins at a fallthrough rather than a branch target.
        // Its exact structural matcher is therefore selected outside the targeted-region gate.
        if fast & 32768 != 0 && rc_ok {
            if let Some(plan) =
                plan_scheduler_handler_deliver(chunk, ops, pc, &cfg, layout, fast)
            {
                let plain_h =
                    emit_scheduler_handler_deliver_region(&mut a, layout, &plan, &pc_labels);
                a.bind(plain_h);
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!(
                        "[jit-region] head {pc}: EMITTED scheduler Handler v2 empty/one-node delivery"
                    );
                }
            }
        }
        // Standalone hot dispatch at the scheduler's active-path exit. This bytecode is a
        // targeted empty-stack join rather than a loop header, so it sits beside (not inside)
        // the loop-region selection below. The plain templates remain the exact replay path.
        if fast & 32768 != 0 && rc_ok && targeted[pc] {
            let fast_resume = if scheduler_dispatch_pc == Some(pc) {
                scheduler_fast_resume
            } else {
                None
            };
            if let Some(plan) = plan_scheduler_handler_queue(chunk, ops, pc, layout, fast) {
                let plain_h =
                    emit_scheduler_handler_queue_region(&mut a, layout, &plan, &pc_labels);
                a.bind(plain_h);
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!(
                        "[jit-region] head {pc}: EMITTED scheduler Handler v1 queue"
                    );
                }
            }
            let device_plan = plan_scheduler_device(chunk, ops, pc, layout, fast);
            let mut handler_plan = plan_scheduler_handler_suspend(chunk, ops, pc, layout, fast);
            if let Some(plan) = handler_plan.as_mut() {
                plan.incoming = plan_scheduler_handler_incoming(
                    chunk,
                    ops,
                    pc,
                    &cfg,
                    layout,
                    fast,
                    plan,
                );
            }

            let pc59_role_dispatch = scheduler_role_epoch
                && scheduler_dispatch_pc == Some(pc)
                && fast_resume.is_some()
                && std::env::var_os("LUMEN_JIT_NO_SCHED_PC59_ROLE_DISPATCH").is_none()
                && device_plan
                    .as_ref()
                    .zip(handler_plan.as_ref())
                    .is_some_and(|(device, handler)| {
                        scheduler_pc59_role_dispatch_compatible(device, handler)
                    });
            let original_dispatch = pc59_role_dispatch.then(|| a.new_label());
            let device_prevalidated = pc59_role_dispatch.then(|| a.new_label());
            let handler_prevalidated = pc59_role_dispatch.then(|| a.new_label());
            if let (Some(original_dispatch), Some(device_target), Some(handler_target)) = (
                original_dispatch,
                device_prevalidated,
                handler_prevalidated,
            ) {
                let device = device_plan.as_ref().expect("pc59 role-compatible Device");
                let handler = handler_plan
                    .as_ref()
                    .expect("pc59 role-compatible Handler");
                emit_scheduler_pc59_role_selector(
                    &mut a,
                    layout,
                    device,
                    handler,
                    device_target,
                    handler_target,
                    original_dispatch,
                );
                a.bind(original_dispatch);
            }
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some()
                && scheduler_dispatch_pc == Some(pc)
            {
                eprintln!(
                    "[jit-region] head {pc}: scheduler pc59 role dispatch={pc59_role_dispatch}"
                );
            }

            if let Some(plan) = device_plan.as_ref() {
                let plain_h = emit_scheduler_device_region(
                    &mut a,
                    layout,
                    plan,
                    device_prevalidated,
                    fast_resume,
                    &pc_labels,
                );
                a.bind(plain_h);
                targeted[plan.suspend_pc] = true;
                targeted[plan.queue_pc] = true;
                targeted[plan.hold_pc] = true;
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!(
                        "[jit-region] head {pc}: EMITTED scheduler Device dispatch (direct_suspend={}, direct_queue={}, direct_hold={})",
                        plan.suspend.is_some(),
                        plan.queue.is_some(),
                        plan.hold.is_some()
                    );
                }
            }
            if let Some(plan) = handler_plan.as_ref() {
                let has_device_delivery = plan.incoming.is_some();
                let plain_h = emit_scheduler_handler_suspend_region(
                    &mut a,
                    layout,
                    plan,
                    handler_prevalidated,
                    fast_resume,
                    &pc_labels,
                );
                a.bind(plain_h);
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!(
                        "[jit-region] head {pc}: EMITTED scheduler Handler wait suspend (incoming_device={has_device_delivery})"
                    );
                }
            }
            // Reaching the ordinary dispatch means every direct, guard-only task arm declined.
            // User code/accessors may run from here, so no epoch fact may survive this boundary.
            if fast_resume.is_some() {
                a.movz(28, 0, 0);
            }
            if scheduler_dispatch_pc == Some(pc) {
                if let Some(plain_dispatch) = scheduler_plain_dispatch {
                    a.bind(plain_dispatch);
                }
            }
        }
        // Loop-spanning chain: a fully-chainable, branch-free loop headed here runs with its
        // locals register-resident across the back edge. The plain templates for the region are
        // still emitted below (starting at `plain_h`) as the bail target; the head's canonical
        // label points at the chain entry, so plain back-edge jumps re-enter the chain.
        if fast & 32768 != 0 && rc_ok && targeted[pc] {
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                match crate::jit_ir::RegionIr::build_loop(chunk, &cfg, pc) {
                    Ok(region) => {
                        let op_count: usize = region.blocks.iter().map(|b| b.insts.len()).sum();
                        let phi_count = region
                            .values
                            .iter()
                            .filter(|v| {
                                matches!(v.def, crate::jit_ir::ValueDef::BlockParam { .. })
                            })
                            .count();
                        eprintln!(
                            "[jit-region] head {pc}: {} blocks, {op_count} ops, {} values, {phi_count} params, {} exits",
                            region.blocks.len(),
                            region.values.len(),
                            region.exits.len()
                        );
                    }
                    // Most targeted bytecodes are ordinary jump destinations rather than loop
                    // headers.  `NoLoop` is therefore expected and would drown out actionable
                    // region diagnostics on large functions such as Richards' scheduler.
                    Err(crate::jit_ir::IrError::NoLoop) => {}
                    Err(err) => eprintln!("[jit-region] head {pc}: reject {err:?}"),
                }
            }
            let mut emitted_region = false;
            let preplanned_fast = fast_scheduler_shell
                .as_ref()
                .is_some_and(|(head, _)| *head == pc);
            let shell_plan = if preplanned_fast {
                fast_scheduler_shell.take().map(|(_, plan)| plan)
            } else {
                plan_scheduler_shell(chunk, ops, pc, &cfg, layout, fast)
            };
            if let Some(plan) = shell_plan {
                // The forward pc59 continuation belongs exclusively to the one preplanned fast
                // scheduler shell. Structurally similar later loop regions must keep local labels.
                let shell_plain_dispatch = preplanned_fast
                    .then_some(scheduler_plain_dispatch)
                    .flatten();
                let (plain_h, fast_resume) = emit_scheduler_shell_region(
                    &mut a,
                    layout,
                    &plan,
                    preplanned_fast,
                    shell_plain_dispatch,
                    &pc_labels,
                );
                a.bind(plain_h);
                if preplanned_fast {
                    a.movz(28, 0, 0);
                    scheduler_fast_resume = fast_resume;
                    scheduler_dispatch_pc = plan.active.as_ref().map(|active| active.exit_pc);
                }
                for p in pc + 1..pc + 28 {
                    targeted[p] = true;
                }
                emitted_region = true;
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    let role_dispatch = std::env::var_os(
                        "LUMEN_JIT_NO_SCHED_ROLE_DISPATCH",
                    )
                    .is_none()
                        && plan
                            .active
                            .as_ref()
                            .and_then(|active| active.null_dispatch.as_ref())
                            .is_some_and(scheduler_active_null_role_dispatch_compatible);
                    let role_epoch = preplanned_fast && scheduler_role_epoch_enabled(&plan);
                    let method_epoch = preplanned_fast && scheduler_method_epoch_enabled(&plan);
                    let graph_epoch =
                        preplanned_fast && scheduler_graph_epoch_enabled(&plan, layout);
                    let graph_core = graph_epoch && scheduler_graph_core_enabled(&plan);
                    let graph_core_incoming =
                        graph_core && scheduler_graph_core_incoming_enabled(&plan);
                    let packet_role_dispatch = graph_epoch
                        && std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH")
                            .is_none();
                    eprintln!(
                        "[jit-region] head {pc}: EMITTED scheduler shell (active={}, active_null_stitch={}, active_role_dispatch={role_dispatch}, active_packet_role_dispatch={packet_role_dispatch}, active_role_epoch={role_epoch}, method_epoch={method_epoch}, graph_epoch={graph_epoch}, graph_core={graph_core}, graph_core_incoming={graph_core_incoming}, active_idle={}, active_worker={}, active_worker_packet={}, fast_loop={preplanned_fast})",
                        plan.active.is_some(),
                        plan.active.as_ref().is_some_and(|active| active.null_dispatch.is_some()),
                        plan.active.as_ref().and_then(|active| active.null_dispatch.as_ref()).is_some_and(|dispatch| dispatch.idle.is_some()),
                        plan.active.as_ref().and_then(|active| active.null_dispatch.as_ref()).is_some_and(|dispatch| dispatch.worker.is_some()),
                        plan.active.as_ref().and_then(|active| active.null_dispatch.as_ref()).and_then(|dispatch| dispatch.worker.as_ref()).is_some_and(|worker| worker.work.is_some()),
                    );
                }
            } else if let Some(plan) = plan_linked_scan(chunk, ops, pc, &cfg, layout, fast) {
                let plain_h = emit_linked_scan_region(&mut a, layout, &plan, &pc_labels);
                a.bind(plain_h);
                for p in pc + 1..pc + 9 {
                    targeted[p] = true;
                }
                emitted_region = true;
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!("[jit-region] head {pc}: EMITTED linked scan");
                }
            } else if let Some(plan) = plan_numeric_diamond(chunk, ops, pc, &cfg, layout, fast) {
                let plain_h = emit_numeric_diamond_region(&mut a, layout, &plan, &pc_labels);
                a.bind(plain_h);
                for p in pc + 1..pc + 18 {
                    targeted[p] = true;
                }
                emitted_region = true;
                if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                    eprintln!("[jit-region] head {pc}: EMITTED numeric diamond");
                }
            }
            if !emitted_region {
                if let Some(plan) = plan_loop(chunk, ops, pc, &targeted, layout, fast, &cfg) {
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
        }
        // Local identity/nullish comparison feeding a branch. Reading the two frame slots
        // non-owningly avoids two Value clones, their stack traffic, the equality helper, and
        // both refcount drops for hot `if (object != excluded)` loops. Unsupported/coercing
        // pairs replay all three value ops plus the condition from untouched state.
        if fast & 2 != 0 && eq_inlinable(layout) && pc + 3 < ops.len() {
            if let (
                Op::LoadLocal(lhs),
                Some(Op::LoadLocal(rhs)),
                Some(cmp @ (Op::StrictEq | Op::StrictNotEq | Op::EqEq | Op::NotEq)),
                Some(Op::JumpIfFalse(target)),
            ) = (
                op,
                ops.get(pc + 1),
                ops.get(pc + 2),
                ops.get(pc + 3),
            ) {
                let lhs_off = *lhs as u32 * 16;
                let rhs_off = *rhs as u32 * 16;
                if !targeted[pc + 1]
                    && !targeted[pc + 2]
                    && !targeted[pc + 3]
                    && lhs_off + 16 < 4096
                    && rhs_off + 16 < 4096
                {
                    emit_local_eq_branch(
                        &mut a,
                        layout,
                        lhs_off,
                        rhs_off,
                        pc as u32,
                        l_unwind,
                        matches!(cmp, Op::StrictEq | Op::StrictNotEq),
                        matches!(cmp, Op::NotEq | Op::StrictNotEq),
                        pc_labels[*target as usize],
                    );
                    skip = 3;
                    continue;
                }
            }
        }
        // Numeric register chain: a run of ops whose values stay in FP registers end to end.
        if fast & 16384 != 0 && rc_ok {
            if let Some((chain, consumed)) = build_chain(chunk, ops, pc, &targeted, layout, fast) {
                emit_chain(&mut a, layout, &chain, &pc_labels, l_unwind);
                skip = consumed - 1;
                continue;
            }
        }
        // Jump-threaded equality condition: short-circuit lowering can produce
        // `Eq; Jump(shared-cond)` where the destination is a JumpIfFalse. Drive both outcomes
        // directly from the equality template, bypassing the temporary Bool, the forwarding
        // jump, and its immediate pop. Other predecessors still enter the shared condition's
        // ordinary template. The skipped forwarding Jump must have no incoming edge of its own.
        if fast & 2 != 0 && eq_inlinable(layout) && !targeted[pc + 1] {
            if let (
                Op::StrictEq | Op::StrictNotEq | Op::EqEq | Op::NotEq,
                Some(Op::Jump(cond_pc)),
            ) = (op, ops.get(pc + 1))
            {
                let cond_pc = *cond_pc as usize;
                if let Some(Op::JumpIfFalse(false_pc)) = ops.get(cond_pc) {
                    emit_eq_inline(
                        &mut a,
                        layout,
                        pc as u32,
                        l_unwind,
                        matches!(op, Op::StrictEq | Op::StrictNotEq),
                        matches!(op, Op::NotEq | Op::StrictNotEq),
                        Some(pc_labels[*false_pc as usize]),
                    );
                    a.b(pc_labels[cond_pc + 1]);
                    skip = 1;
                    continue;
                }
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
                Op::Lt
                | Op::Gt
                | Op::Le
                | Op::Ge
                | Op::StrictEq
                | Op::StrictNotEq
                | Op::EqEq
                | Op::NotEq,
                Some(Op::JumpIfFalse(t)),
            ) = (op, ops.get(pc + 1))
            {
                if !targeted[pc + 1] {
                    let neg = match op {
                        Op::Lt => 5,                  // PL: !(a<b), true for unordered (NaN must jump)
                        Op::Gt => 13,                 // LE: !(a>b), true for unordered
                        Op::Le => 8,                  // HI: !(a<=b), true for unordered
                        Op::Ge => 11,                 // LT: !(a>=b), true for unordered
                        Op::StrictEq | Op::EqEq => 1, // NE: !(a==b), true for unordered
                        _ => 0, // EQ: !(a!=b); unordered IS "!=" → correctly no jump
                    };
                    let slow = a.new_label();
                    let done = a.new_label();
                    a.ldurb(9, 20, -32);
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_NE, slow);
                    a.ldurb(9, 20, -16);
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_NE, slow);
                    a.ldur_d(0, 20, -24);
                    a.ldur_d(1, 20, -8);
                    a.sub_imm(20, 20, 32); // pop both operands (no bool pushed)
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
        if fast & 1024 != 0 && get_elem_inlinable(layout) && !targeted[pc + 1] {
            let in_range = |s: u16| (s as u32) * 16 + 16 < 4096;
            let pair = match (op, ops.get(pc + 1)) {
                (Op::LoadLocal(k), Some(Op::GetElemLocal(x))) if in_range(*k) && in_range(*x) => {
                    Some((*x as u32 * 16, KeySrc::Slot(*k as u32 * 16)))
                }
                (
                    Op::UpdateLocal(k, kind @ (UpdKind::PreInc | UpdKind::PreDec)),
                    Some(Op::GetElemLocal(x)),
                ) if in_range(*k) && in_range(*x) => Some((
                    *x as u32 * 16,
                    KeySrc::SlotPre(*k as u32 * 16, matches!(kind, UpdKind::PreDec)),
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
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 3);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -15); // bool payload at offset 1
                a.sub_imm(20, 20, 16);
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
                emit_peek_cond_inline(&mut a, layout, false, l_unwind);
                a.cbz(1, false, pc_labels[*t as usize]);
            }
            Op::JumpIfTruePeek(t) => {
                emit_peek_cond_inline(&mut a, layout, false, l_unwind);
                a.cbnz(1, false, pc_labels[*t as usize]);
            }
            Op::JumpIfNotNullishPeek(t) => {
                emit_peek_cond_inline(&mut a, layout, true, l_unwind);
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
                    ilayout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_cache_preferred(*cache),
                    chunk.jit_name(*n),
                    pc as u32,
                    l_unwind,
                    false,
                    arr_ok,
                    PropRecv::Stack,
                );
            }
            // Receiver-direct reads (`this.x`, `slotlocal.x`): the receiver never crosses the
            // operand stack and needs no refcounting (the frame owns it).
            Op::GetPropThis(n, cache) if fast & 256 != 0 && get_method_inlinable(layout) => {
                let arr_ok = !chunk
                    .jit_name(*n)
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit());
                emit_prop_load_inline(
                    &mut a,
                    layout,
                    ilayout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_cache_preferred(*cache),
                    chunk.jit_name(*n),
                    pc as u32,
                    l_unwind,
                    false,
                    arr_ok,
                    PropRecv::This,
                );
            }
            Op::GetPropLocal(s, n, cache)
                if fast & 256 != 0
                    && get_method_inlinable(layout)
                    && (*s as u32) * 16 + 16 < 4096 =>
            {
                let arr_ok = !chunk
                    .jit_name(*n)
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit());
                emit_prop_load_inline(
                    &mut a,
                    layout,
                    ilayout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_cache_preferred(*cache),
                    chunk.jit_name(*n),
                    pc as u32,
                    l_unwind,
                    false,
                    arr_ok,
                    PropRecv::Slot(*s as u32 * 16),
                );
            }
            Op::ToPropKey | Op::ToPropKeyLocal(_) if fast & 64 != 0 => {
                // A Num or Str key passes through untouched (the overwhelmingly common case);
                // anything else — real coercion plus the nullish-base check — takes the helper.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -16);
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
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 5);
                a.b_cond(C_EQ, slow);
                a.ldur(10, 20, -16);
                a.ldur(11, 20, -8);
                a.stur(10, 20, 0);
                a.stur(11, 20, 8);
                let nobump = a.new_label();
                a.cmp_imm_w(9, 6);
                a.b_cond(C_LO, nobump);
                a.ldur(13, 11, rc_strong);
                a.add_imm(13, 13, 1);
                a.stur(13, 11, rc_strong);
                a.bind(nobump);
                a.add_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::LoadThis if fast & 32768 != 0 && rc_ok => {
                // Copy ctx.this_val (16 bytes) and bump its refcount inline; only a BigInt
                // `this` (impossible in practice, but be safe) takes the helper.
                let slow = a.new_label();
                let done = a.new_label();
                a.ldr_imm(9, 19, 48); // ctx.this_raw
                a.ldrb_imm(10, 9, 0);
                a.cmp_imm_w(10, 5);
                a.b_cond(C_EQ, slow);
                a.ldr_imm(11, 9, 0);
                a.ldr_imm(12, 9, 8);
                a.stur(11, 20, 0);
                a.stur(12, 20, 8);
                let nobump = a.new_label();
                a.cmp_imm_w(10, 6);
                a.b_cond(C_LO, nobump);
                a.ldur(14, 12, rc_strong);
                a.add_imm(14, 14, 1);
                a.stur(14, 12, rc_strong);
                a.bind(nobump);
                a.add_imm(20, 20, 16);
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
                    chunk.jit_name_number(*cache),
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
                    chunk.jit_name_number(*cache),
                    pc as u32,
                    l_unwind,
                    true,
                );
            }
            Op::UpdateNameCached(_, cache, kind)
                if fast & 8192 != 0 && update_name_inlinable(layout) =>
            {
                emit_update_name_inline(
                    &mut a,
                    layout,
                    chunk.jit_name_cache_ptr(*cache),
                    *kind,
                    pc as u32,
                    l_unwind,
                );
            }
            Op::StoreNameCached(_, cache)
                if fast & 8192 != 0 && update_name_inlinable(layout) =>
            {
                emit_store_name_inline(
                    &mut a,
                    layout,
                    chunk.jit_name_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                );
            }
            // ---- inline dense-element fast paths (`a[i]` on plain objects/arrays) ----
            Op::GetElem if fast & 1024 != 0 && get_elem_inlinable(layout) => {
                emit_get_elem_inline(&mut a, layout, pc as u32, l_unwind);
            }
            Op::SetElemDrop
                if fast & 2048 != 0
                    && elem_inlinable(layout) =>
            {
                emit_set_elem_inline(&mut a, layout, pc as u32, l_unwind, false);
            }
            Op::SetElem if fast & 4096 != 0 && elem_inlinable(layout) => {
                emit_set_elem_inline(&mut a, layout, pc as u32, l_unwind, true);
            }
            // ---- fused parameter-slot element ops (no receiver stack traffic or refcounting) ----
            Op::GetElemLocal(slot)
                if fast & 1024 != 0
                    && get_elem_inlinable(layout)
                    && (*slot as u32) * 16 + 16 < 4096 =>
            {
                emit_elem_local_inline(
                    &mut a,
                    layout,
                    *slot as u32 * 16,
                    pc as u32,
                    l_unwind,
                    ElemLocalKind::Get,
                );
            }
            Op::SetElemLocalDrop(slot)
                if fast & 2048 != 0
                    && elem_inlinable(layout)
                    // Packed local stores are enabled with the complete numeric-loop pipeline.
                    // In reduced diagnostic masks, mixing this baseline store with helper-side
                    // name/element state can violate Navier's aliasing checksum; the old wide
                    // property layout remains independently safe.
                    && (layout.entry_accessor != layout.entry_value + 8
                        || fast & (1024 | 8192 | 32768 | 262144)
                            == (1024 | 8192 | 32768 | 262144))
                    && (*slot as u32) * 16 + 16 < 4096 =>
            {
                emit_elem_local_inline(
                    &mut a,
                    layout,
                    *slot as u32 * 16,
                    pc as u32,
                    l_unwind,
                    ElemLocalKind::SetDrop,
                );
            }
            Op::SetElemLocal(slot)
                if fast & 4096 != 0
                    && elem_inlinable(layout)
                    && (*slot as u32) * 16 + 16 < 4096 =>
            {
                emit_elem_local_inline(
                    &mut a,
                    layout,
                    *slot as u32 * 16,
                    pc as u32,
                    l_unwind,
                    ElemLocalKind::SetKeep,
                );
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
                    ilayout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_cache_preferred(*cache),
                    chunk.jit_name(*n),
                    pc as u32,
                    l_unwind,
                    true,
                    arr_ok,
                    PropRecv::Stack,
                );
            }
            // ---- inline fast paths (tags: 3 = Bool, 4 = Num; payload at +8; Value = 16) ----
            Op::Add | Op::Sub | Op::Mul | Op::Div if fast & 1 != 0 => {
                let f_op = match op {
                    Op::Add => 0,
                    Op::Sub => 1,
                    Op::Mul => 2,
                    _ => 3,
                };
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -32);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -24);
                a.ldur_d(1, 20, -8);
                a.f_arith(f_op, 0, 0, 1);
                a.stur_d(0, 20, -24);
                a.sub_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            // Int32 ops on two numbers: ToInt32 = truncate + wrap to 32 bits. fcvtzs to x
            // truncates; taking the low 32 bits is the mod-2^32 wrap. The scvtf/frintz
            // round-trip proves no i64 saturation happened (NaN/±Inf/|x|≥2^63 all fail it and
            // take the helper, which applies the spec's zero/wrap semantics).
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr if fast & 1 != 0 => {
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -32);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -24); // lhs
                a.ldur_d(1, 20, -8); // rhs
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
                a.stur_d(0, 20, -24);
                a.sub_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            // Unary ToInt32 + complement. Keep the same exactness/saturation proof as the
            // binary int templates: fcvtzs gives us the modulo source only when its i64 result
            // round-trips to the truncated input. NaN, infinities, huge numbers, objects and
            // BigInts retain the generic coercion/throw path.
            Op::BitNot if fast & 1 != 0 => {
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -8);
                a.fcvtzs_x_d(9, 0);
                a.scvtf_d_x(1, 9);
                a.frintz(2, 0);
                a.fcmp(1, 2);
                a.b_cond(C_NE, slow);
                a.cmn_imm_x(9, 1);
                a.b_cond(C_VS, slow);
                a.mvn_w(10, 9);
                a.scvtf_d_w(0, 10);
                a.stur_d(0, 20, -8);
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
            Op::InstanceOf(cache) if rc_ok && instanceof_inlinable(layout, ilayout) => {
                emit_instanceof_inline(
                    &mut a,
                    layout,
                    ilayout,
                    chunk.jit_cache_ptr(*cache),
                    pc as u32,
                    l_unwind,
                );
            }
            Op::Not if fast & 131072 != 0 && eq_inlinable(layout) => {
                emit_not_inline(&mut a, layout, pc as u32, l_unwind);
            }
            Op::SetPropDrop(_, cache)
                if fast & 65536 != 0 && rc_ok && set_prop_inlinable(layout) =>
            {
                emit_set_prop_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_name(match op { Op::SetPropDrop(n, _) => *n, _ => unreachable!() }),
                    pc as u32,
                    l_unwind,
                    PropRecv::Stack,
                );
            }
            // Receiver-direct stores (`this.x = v`, `slotlocal.x = v`): the receiver never
            // crosses the operand stack and needs no refcounting (the frame owns it).
            Op::SetPropThisDrop(_, cache)
                if fast & 65536 != 0 && rc_ok && set_prop_inlinable(layout) =>
            {
                emit_set_prop_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_name(match op { Op::SetPropThisDrop(n, _) => *n, _ => unreachable!() }),
                    pc as u32,
                    l_unwind,
                    PropRecv::This,
                );
            }
            Op::SetPropLocalDrop(s, _, cache)
                if fast & 65536 != 0
                    && rc_ok
                    && set_prop_inlinable(layout)
                    && (*s as u32) * 16 + 16 < 4096 =>
            {
                emit_set_prop_inline(
                    &mut a,
                    layout,
                    chunk.jit_cache_ptr(*cache),
                    chunk.jit_name(match op { Op::SetPropLocalDrop(_, n, _) => *n, _ => unreachable!() }),
                    pc as u32,
                    l_unwind,
                    PropRecv::Slot(*s as u32 * 16),
                );
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
            Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::EqEq
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
                a.ldurb(9, 20, -32);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldurb(9, 20, -16);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, slow);
                a.ldur_d(0, 20, -24);
                a.ldur_d(1, 20, -8);
                a.fcmp(0, 1);
                a.cset_w(9, cond);
                a.movz(10, 3, 0); // Bool tag word (payload byte 1 zeroed by the 64-bit store)
                a.sub_imm(20, 20, 16);
                a.stur(10, 20, -16);
                a.sturb(9, 20, -15); // bool payload at offset 1
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::LoadLocal(slot) if fast & 8 != 0 && (*slot as u32) * 16 + 16 < 4096 => {
                let off = *slot as u32 * 16;
                let slow = a.new_label();
                let done = a.new_label();
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 1); // Empty = TDZ throw → slow
                a.b_cond(C_EQ, slow);
                if rc_ok {
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_EQ, slow);
                    a.ldr_imm(10, 22, off);
                    a.ldr_imm(11, 22, off + 8);
                    a.stur(10, 20, 0);
                    a.stur(11, 20, 8);
                    let nobump = a.new_label();
                    a.cmp_imm_w(9, 6);
                    a.b_cond(C_LO, nobump);
                    a.ldur(13, 11, rc_strong);
                    a.add_imm(13, 13, 1);
                    a.stur(13, 11, rc_strong);
                    a.bind(nobump);
                } else {
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_HI, slow);
                    a.ldr_imm(10, 22, off);
                    a.ldr_imm(11, 22, off + 8);
                    a.stur(10, 20, 0);
                    a.stur(11, 20, 8);
                }
                a.add_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::StoreLocal(slot) if fast & 16 != 0 && (*slot as u32) * 16 + 16 < 4096 => {
                let off = *slot as u32 * 16;
                let slow = a.new_label();
                let done = a.new_label();
                a.ldrb_imm(9, 22, off);
                if rc_ok {
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
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_HI, slow);
                }
                a.ldur(9, 20, -16);
                a.ldur(10, 20, -8);
                a.str_imm(9, 22, off);
                a.str_imm(10, 22, off + 8);
                a.sub_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::UpdateLocal(slot, kind) if fast & 32 != 0 && (*slot as u32) * 16 + 8 < 4096 => {
                let off = *slot as u32 * 16;
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
                        a.add_imm(20, 20, 16);
                    }
                    UpdKind::PostInc | UpdKind::PostDec => {
                        a.movz(10, 4, 0);
                        a.stur(10, 20, 0);
                        a.stur_d(0, 20, 8);
                        a.add_imm(20, 20, 16);
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
                a.ldurb(9, 20, -16);
                if rc_ok {
                    // A refcounted top drops inline (strong--) unless it is the last reference
                    // (real destructor) or a BigInt (compound payload) — those take the helper.
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_EQ, slow);
                    let plain = a.new_label();
                    a.cmp_imm_w(9, 6);
                    a.b_cond(C_LO, plain);
                    a.ldur(10, 20, -8);
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
                a.sub_imm(20, 20, 16);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Undef if fast & 128 != 0 => {
                a.stur(31, 20, 0);
                a.stur(31, 20, 8);
                a.add_imm(20, 20, 16);
            }
            Op::Const(k) if fast & 128 != 0 && chunk.jit_const_copyable(*k) => {
                let (word0, word1) = chunk.jit_const_bits(*k);
                a.mov_imm64(9, word0);
                a.stur(9, 20, 0);
                a.mov_imm64(9, word1);
                a.stur(9, 20, 8);
                a.add_imm(20, 20, 16);
            }
            // String consts: copy the 16-byte Value from its stable chunk slot and bump the
            // LStr strong count — parser-shaped code pushes literal strings millions of times
            // (meriyah's token kinds, astring's syntax fragments).
            Op::Const(k)
                if fast & 128 != 0
                    && rc_ok
                    && layout.rc_strong_off == 0
                    && chunk.jit_const_is_str(*k) =>
            {
                a.mov_imm64(9, chunk.jit_const_ptr(*k) as u64);
                a.ldr_imm(10, 9, 0);
                a.ldr_imm(11, 9, 8);
                a.stur(10, 20, 0);
                a.stur(11, 20, 8);
                a.ldur(13, 11, 0); // strong (payload+0)
                a.add_imm(13, 13, 1);
                a.stur(13, 11, 0);
                a.add_imm(20, 20, 16);
            }
            // TDZ entry: the slot becomes `Empty` (tag 1). The old value drops in place —
            // trivially for tags < 5, by a bare shared-reference decrement for refcounted
            // tags; a BigInt or a last reference re-runs the (idempotent) op via the helper.
            // Destructuring nullish guard: a pure peek — tag not Undefined(0)/Null(2) falls
            // through with no stack traffic; nullish re-runs the op via the helper, which
            // throws the TypeError. Three instructions instead of 3.5M helper trips on
            // destructuring-heavy parses.
            Op::DestructureGuard => {
                let slow = a.new_label();
                let done = a.new_label();
                a.ldurb(9, 20, -16);
                a.cbz(9, false, slow); // Undefined
                a.cmp_imm_w(9, 2);
                a.b_cond(C_NE, done); // anything but Null
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            Op::Tdz(slot) if fast & 16 != 0 && rc_ok && (*slot as u32) * 16 + 16 < 4096 => {
                let off = *slot as u32 * 16;
                let slow = a.new_label();
                let done = a.new_label();
                let plain = a.new_label();
                a.ldrb_imm(9, 22, off);
                a.cmp_imm_w(9, 5);
                a.b_cond(C_LO, plain);
                a.b_cond(C_EQ, slow);
                a.ldr_imm(10, 22, off + 8);
                a.ldur(11, 10, rc_strong);
                a.cmp_imm_x(11, 1);
                a.b_cond(C_LS, slow);
                a.sub_imm(11, 11, 1);
                a.stur(11, 10, rc_strong);
                a.bind(plain);
                a.movz(9, 1, 0);
                a.strb_imm(9, 22, off);
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            // Fused slot resets (spliced-callee var hoisting): plain tags overwrite in place
            // with a single byte store; refcounted values (the receiver-array vars of a spliced
            // bignum kernel, typically) decrement inline while shared. Only a last-reference
            // drop (or a string-ish tag 5) re-runs the whole (idempotent) op via the helper.
            Op::ResetSlots(start, count)
                if rc_ok && (*start as u32 + *count as u32) * 16 < 4096 =>
            {
                let slow = a.new_label();
                let done = a.new_label();
                for k in *start..*start + *count {
                    let off = k as u32 * 16;
                    let plain = a.new_label();
                    a.ldrb_imm(9, 22, off);
                    a.cmp_imm_w(9, 5);
                    a.b_cond(C_LO, plain);
                    a.b_cond(C_EQ, slow);
                    a.ldr_imm(10, 22, off + 8);
                    a.ldur(11, 10, rc_strong);
                    a.cmp_imm_x(11, 1);
                    a.b_cond(C_LS, slow);
                    a.sub_imm(11, 11, 1);
                    a.stur(11, 10, rc_strong);
                    a.bind(plain);
                    a.strb_imm(31, 22, off);
                }
                a.b(done);
                a.bind(slow);
                emit_exec(&mut a, pc as u32, l_unwind);
                a.bind(done);
            }
            // Speculative-inline guard: the callee (argc+1 deep) must be the pinned function —
            // a tag compare and a pointer compare; mismatch branches to the generic call.
            Op::InlineGuard(t, target) => {
                let it = chunk.jit_inline_target(*t);
                // A Value::Obj payload holds the STORED Rc pointer (the RcBox base), not
                // `Rc::as_ptr` — read the expected stored word out of an Option<Gc> exactly
                // like `value::jit_layout` probes it. A dead callee (or an unprobed layout)
                // degrades to the generic call unconditionally.
                let stored = it.pin.upgrade().filter(|_| layout.valid).map(|o| {
                    let some: Option<crate::value::Gc> = Some(o);
                    unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
                });
                match stored {
                    None => a.b(pc_labels[*target as usize]),
                    Some(s) => {
                        let dm = (it.argc as i32 + 1) * 16;
                        a.ldurb(9, 20, -dm);
                        a.cmp_imm_w(9, 8);
                        a.b_cond(C_NE, pc_labels[*target as usize]);
                        a.ldur(9, 20, -dm + 8);
                        a.mov_imm64(10, s as u64);
                        a.cmp_reg_x(9, 10);
                        a.b_cond(C_NE, pc_labels[*target as usize]);
                        if it.check_this {
                            a.ldurb(9, 20, -dm - 16);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, pc_labels[*target as usize]);
                        }
                    }
                }
            }
            // Calls take the dedicated helper: same contract as the generic one, minus the full
            // op dispatch (they dominate helper traffic in call-heavy code). With bit 524288,
            // the way-1 identity probe runs inline first — callee payload vs the cached
            // pointer, fill epoch vs the live CALL_IC_EPOCH, fill realm vs the activation's
            // global root — and a hit takes the H_CALL_HIT helper, which skips the probe loop
            // and op decode. Any mismatch (incl. an empty way: callee 0 matches no payload)
            // falls to the full helper.
            Op::Call(argc, c) | Op::CallWithThis(argc, c) => {
                let inline_probe = fast & 524288 != 0;
                let slow = a.new_label();
                let done = a.new_label();
                if inline_probe {
                    let depth = *argc as u32 + 1; // callee sits under the args
                    let off = depth as i32 * -16;
                    if (-256..0).contains(&off) {
                        let ic0 = chunk.jit_call_cache_ptr(*c);
                        a.ldurb(9, 20, off);
                        a.cmp_imm_w(9, 8); // callee must be an Obj
                        a.b_cond(C_NE, slow);
                        a.ldur(10, 20, off + 8); // callee payload (stored Rc ptr)
                        // the payload is the STORED RcBox pointer; as_ptr sits one probed
                        // header further (comparing them raw was a silent 100% miss)
                        a.add_imm(13, 10, layout.gc_data_off as u32);
                        // Probe ALL 4 ways (a stable polymorphic site — e.g. one dispatch
                        // loop over a handful of receiver classes — otherwise pays the full
                        // helper on every call that isn't way 1): x12 = entry cursor,
                        // w14 = ways left, w15 = live epoch, x17 = ctx.genv.
                        a.mov_imm64(12, ic0 as u64);
                        a.mov_imm64(11, &crate::bytecode::CALL_IC_EPOCH as *const _ as u64);
                        a.ldr_w_imm(15, 11, 0);
                        a.ldr_imm(17, 19, 64); // ctx.genv
                        a.movz(14, crate::bytecode::CALL_IC_WAYS as u32, 0);
                        let l_probe = a.new_label();
                        let l_next = a.new_label();
                        let l_hit = a.new_label();
                        a.bind(l_probe);
                        a.ldur(11, 12, 0); // ic.callee (an Rc::as_ptr identity)
                        a.cmp_reg_x(13, 11);
                        a.b_cond(C_NE, l_next);
                        a.ldr_w_imm(11, 12, 56); // ic.epoch
                        a.cmp_reg_w(11, 15);
                        a.b_cond(C_NE, l_next);
                        a.ldr_imm(11, 12, 32); // ic.global_env
                        a.cmp_reg_x(11, 17);
                        a.b_cond(C_EQ, l_hit);
                        a.bind(l_next);
                        // entry stride (size compile-asserted below the JitCtx asserts)
                        let stride =
                            std::mem::size_of::<std::cell::Cell<crate::bytecode::CallIc>>();
                        a.add_imm(12, 12, stride as u32);
                        a.sub_imm(14, 14, 1);
                        a.cbnz(14, false, l_probe);
                        a.b(slow);
                        // x15 = the hit way (ways-left counter → index), kept live through
                        // the direct sequence's NO-MUTATION gate checks (they never touch
                        // x15; every route into hit_slow happens before any blr).
                        a.bind(l_hit);
                        a.movz(15, crate::bytecode::CALL_IC_WAYS as u32, 0);
                        a.sub_reg(15, 15, 14);
                        let with_this = matches!(op, Op::CallWithThis(..));
                        let hit_slow = a.new_label();
                        // Inline intrinsics: a native entry the template can finish without
                        // leaving machine code. charCodeAt on a known-ASCII receiver with an
                        // exact in-bounds u32 index is a byte load — meriyah-style scanners
                        // make millions of these per parse. Any miss (intrinsic id, receiver
                        // tag/hint, index shape, bounds, last-reference operands) takes the
                        // H_CALL_HIT form, whose Rust side handles native entries generally.
                        if with_this && *argc == 1 && rc_ok && layout.rc_strong_off == 0 {
                            let char_code = a.new_label();
                            let sqrt = a.new_label();
                            let array_push = array_intrinsics_on.then(|| a.new_label());
                            let no_intr = a.new_label();
                            a.ldrb_imm(9, 12, 96); // ic.intrinsic (offset compile-asserted)
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_CHAR_CODE_AT as u32);
                            a.b_cond(C_EQ, char_code);
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_MATH_SQRT as u32);
                            a.b_cond(C_EQ, sqrt);
                            if let Some(array_push) = array_push {
                                a.cmp_imm_w(9, crate::bytecode::INTRINSIC_ARRAY_PUSH as u32);
                                a.b_cond(C_EQ, array_push);
                            }
                            a.b(no_intr);

                            a.bind(char_code);
                            // receiver: Str with the ASCII hint
                            a.ldurb(9, 20, -48);
                            a.cmp_imm_w(9, 6);
                            a.b_cond(C_NE, hit_slow);
                            a.ldur(11, 20, -40);
                            a.ldr_w_imm(14, 11, crate::lstr::CAP_OFF as u32);
                            a.lsr_imm(14, 14, 31);
                            a.cbz(14, false, hit_slow);
                            // index: exact u32 Num
                            a.ldurb(9, 20, -16);
                            a.cmp_imm_w(9, 4);
                            a.b_cond(C_NE, hit_slow);
                            a.ldur_d(0, 20, -8);
                            a.fcvtzu_w_d(9, 0);
                            a.ucvtf_d_w(1, 9);
                            a.fcmp(0, 1);
                            a.b_cond(C_NE, hit_slow);
                            // bounds (ASCII: byte index == unit index); OOB answers NaN in the
                            // helper
                            a.ldr_w_imm(14, 11, crate::lstr::LEN_OFF as u32);
                            a.cmp_reg_x(9, 14);
                            a.b_cond(C_HS, hit_slow);
                            // both refcounted operands must survive a bare dec
                            a.ldur(14, 11, 0);
                            a.cmp_imm_x(14, 1);
                            a.b_cond(C_LS, hit_slow);
                            a.ldur(13, 10, 0);
                            a.cmp_imm_x(13, 1);
                            a.b_cond(C_LS, hit_slow);
                            // ---- commit: byte load, decs, Num over the receiver slot ----
                            a.add_imm(16, 11, crate::lstr::DATA_OFF as u32);
                            a.ldrb_reg(16, 16, 9);
                            a.ucvtf_d_w(0, 16);
                            a.sub_imm(14, 14, 1);
                            a.stur(14, 11, 0);
                            a.sub_imm(13, 13, 1);
                            a.stur(13, 10, 0);
                            a.movz(9, 4, 0);
                            a.stur(9, 20, -48);
                            a.stur_d(0, 20, -40);
                            a.sub_imm(20, 20, 32);
                            a.b(done);

                            // Math.sqrt(number): the call IC already proved builtin identity.
                            // The receiver is ignored semantically; require an object so it and
                            // the distinct function handle can be released by guarded decrements.
                            a.bind(sqrt);
                            a.ldurb(9, 20, -48);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.ldurb(9, 20, -16);
                            a.cmp_imm_w(9, 4);
                            a.b_cond(C_NE, hit_slow);
                            a.ldur(11, 20, -40);
                            a.ldur(14, 11, 0);
                            a.cmp_imm_x(14, 1);
                            a.b_cond(C_LS, hit_slow);
                            a.ldur(13, 10, 0);
                            a.cmp_imm_x(13, 1);
                            a.b_cond(C_LS, hit_slow);
                            a.ldur_d(0, 20, -8);
                            a.fsqrt(0, 0);
                            a.sub_imm(14, 14, 1);
                            a.stur(14, 11, 0);
                            a.sub_imm(13, 13, 1);
                            a.stur(13, 10, 0);
                            a.movz(9, 4, 0);
                            a.stur(9, 20, -48);
                            a.stur_d(0, 20, -40);
                            a.sub_imm(20, 20, 32);
                            a.b(done);

                            // Array#push(value): builtin identity is proven by the call IC. The
                            // helper moves `value` into dense storage after live array/prototype/
                            // length guards, and restores the operand before the exact builtin on
                            // any miss.
                            if let Some(array_push) = array_push {
                                a.bind(array_push);
                                a.ldurb(9, 20, -48);
                                a.cmp_imm_w(9, 8);
                                a.b_cond(C_NE, hit_slow);
                                a.mov(0, 19);
                                a.movz(1, pc as u32, 0);
                                a.movk(
                                    1,
                                    crate::bytecode::INTRINSIC_ARRAY_PUSH as u32,
                                    1,
                                );
                                a.mov(2, 20);
                                a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                                a.blr(16);
                                a.mov(20, 0);
                                a.cbnz(1, false, l_unwind);
                                a.b(done);
                            }

                            a.bind(no_intr);
                        }
                        if with_this && *argc == 0 && array_intrinsics_on {
                            let no_intr = a.new_label();
                            a.ldrb_imm(9, 12, 96);
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_ARRAY_POP as u32);
                            a.b_cond(C_NE, no_intr);
                            a.ldurb(9, 20, -32);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.mov(0, 19);
                            a.movz(1, pc as u32, 0);
                            a.movk(
                                1,
                                crate::bytecode::INTRINSIC_ARRAY_POP as u32,
                                1,
                            );
                            a.mov(2, 20);
                            a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                            a.blr(16);
                            a.mov(20, 0);
                            a.cbnz(1, false, l_unwind);
                            a.b(done);
                            a.bind(no_intr);
                        }
                        if with_this
                            && (1..=8).contains(argc)
                            && function_call_intrinsic_on
                        {
                            let no_intr = a.new_label();
                            a.ldrb_imm(9, 12, 96);
                            a.cmp_imm_w(
                                9,
                                crate::bytecode::INTRINSIC_FUNCTION_CALL as u32,
                            );
                            a.b_cond(C_NE, no_intr);
                            let receiver_off = -((*argc as i32 + 2) * 16);
                            a.ldurb(9, 20, receiver_off);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.mov(0, 19);
                            a.movz(1, pc as u32, 0);
                            a.movk(
                                1,
                                crate::bytecode::INTRINSIC_FUNCTION_CALL as u32
                                    | ((*argc as u32) << 8),
                                1,
                            );
                            a.mov(2, 20);
                            a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                            a.blr(16);
                            a.mov(20, 0);
                            a.cbnz(1, false, l_unwind);
                            a.b(done);
                            a.bind(no_intr);
                        }
                        if with_this && *argc == 2 {
                            let slice = a.new_label();
                            let has_own = a.new_label();
                            let apply = a.new_label();
                            let no_intr = a.new_label();
                            a.ldrb_imm(9, 12, 96);
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_STRING_SLICE as u32);
                            a.b_cond(C_EQ, slice);
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_OBJECT_HAS_OWN as u32);
                            a.b_cond(C_EQ, has_own);
                            a.cmp_imm_w(9, crate::bytecode::INTRINSIC_FUNCTION_APPLY as u32);
                            a.b_cond(C_EQ, apply);
                            a.b(no_intr);

                            // ASCII String#slice(start, end), both bounds already Numbers: no
                            // user code or exotic conversion can run in the dedicated helper.
                            a.bind(slice);
                            a.ldurb(9, 20, -64);
                            a.cmp_imm_w(9, 6);
                            a.b_cond(C_NE, hit_slow);
                            a.ldur(11, 20, -56);
                            a.ldr_w_imm(9, 11, crate::lstr::CAP_OFF as u32);
                            a.lsr_imm(9, 9, 31);
                            a.cbz(9, false, hit_slow);
                            for off in [-32i32, -16] {
                                a.ldurb(9, 20, off);
                                a.cmp_imm_w(9, 4);
                                a.b_cond(C_NE, hit_slow);
                            }
                            a.mov(0, 19);
                            a.movz(1, pc as u32, 0);
                            a.movk(
                                1,
                                crate::bytecode::INTRINSIC_STRING_SLICE as u32,
                                1,
                            );
                            a.mov(2, 20);
                            a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                            a.blr(16);
                            a.mov(20, 0);
                            a.cbnz(1, false, l_unwind);
                            a.b(done);

                            // Object.hasOwn(obj, string): the named intrinsic's implementation
                            // is exactly an own-map lookup for this non-coercing argument shape.
                            a.bind(has_own);
                            a.ldurb(9, 20, -32);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.ldurb(9, 20, -16);
                            a.cmp_imm_w(9, 6);
                            a.b_cond(C_NE, hit_slow);
                            a.mov(0, 19);
                            a.movz(1, pc as u32, 0);
                            a.movk(
                                1,
                                crate::bytecode::INTRINSIC_OBJECT_HAS_OWN as u32,
                                1,
                            );
                            a.mov(2, 20);
                            a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                            a.blr(16);
                            a.mov(20, 0);
                            a.cbnz(1, false, l_unwind);
                            a.b(done);

                            // Function#apply(targetThis, arguments): builtin identity is already
                            // proven. Restrict the intrinsic helper to an object target and
                            // object list; it performs the ordinary/unmapped/dense guards before
                            // moving entries directly into a compiled target frame.
                            a.bind(apply);
                            a.ldurb(9, 20, -64);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.ldurb(9, 20, -16);
                            a.cmp_imm_w(9, 8);
                            a.b_cond(C_NE, hit_slow);
                            a.mov(0, 19);
                            a.movz(1, pc as u32, 0);
                            a.movk(
                                1,
                                crate::bytecode::INTRINSIC_FUNCTION_APPLY as u32,
                                1,
                            );
                            a.mov(2, 20);
                            a.ldr_imm(16, 21, (H_INTRINSIC * 8) as u32);
                            a.blr(16);
                            a.mov(20, 0);
                            a.cbnz(1, false, l_unwind);
                            a.b(done);
                            a.bind(no_intr);
                        }
                        // Direct shared-ctx call: its own gate misses land on `hit_slow` =
                        // the H_CALL_HIT form below.
                        if direct_on {
                            let attempted_off = chunk.jit_inline_attempted_off();
                            emit_direct_call(
                                &mut a,
                                ilayout,
                                layout.gc_data_off,
                                attempted_off,
                                *argc as usize,
                                with_this,
                                hit_slow,
                                slow,
                                l_unwind,
                                done,
                                l_direct_finish,
                            );
                        }
                        a.bind(hit_slow);
                        a.mov(0, 19);
                        // x1 = pc | way << 16 (pcs are < 65536: every helper call encodes
                        // the pc as one movz)
                        a.movz(1, pc as u32, 0);
                        a.add_shifted(1, 1, 15, 16);
                        a.mov(2, 20);
                        a.ldr_imm(16, 21, (H_CALL_HIT * 8) as u32);
                        a.blr(16);
                        a.mov(20, 0);
                        a.cbnz(1, false, l_unwind);
                        a.b(done);
                    }
                }
                a.bind(slow);
                a.mov(0, 19);
                a.movz(1, pc as u32, 0);
                a.mov(2, 20);
                a.ldr_imm(16, 21, (H_CALL * 8) as u32);
                a.blr(16);
                a.mov(20, 0);
                a.cbnz(1, false, l_unwind);
                a.bind(done);
            }
            Op::MakeObject(..) => {
                emit_op_helper(&mut a, H_MAKE_OBJECT, pc as u32, l_unwind);
            }
            Op::New(argc) => {
                // H_NEW needs only the pc for optional diagnostics and the statically encoded
                // arity. Pack both so the million-call constructor path does not reload/decode
                // its bytecode op in Rust.
                a.mov(0, 19);
                a.movz(1, pc as u32, 0);
                a.movk(1, *argc as u32, 1);
                a.mov(2, 20);
                a.ldr_imm(16, 21, (H_NEW * 8) as u32);
                a.blr(16);
                a.mov(20, 0);
                a.cbnz(1, false, l_unwind);
            }
            Op::SetProp(..)
            | Op::SetPropDrop(..)
            | Op::SetPropThisDrop(..)
            | Op::SetPropLocalDrop(..) => {
                emit_op_helper(&mut a, H_SET_PROP, pc as u32, l_unwind);
            }
            Op::GetProp(..) | Op::GetPropThis(..) | Op::GetPropLocal(..) | Op::GetMethod(..) => {
                emit_op_helper(&mut a, H_GET_PROP, pc as u32, l_unwind);
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
    a.ldp_d_off(8, 9, dsave);
    a.ldp_d_off(10, 11, dsave + 16);
    a.ldp_d_off(12, 13, dsave + 32);
    a.ldp_d_off(14, 15, dsave + 48);
    if scheduler_frame {
        a.ldp_off(23, 24, 48);
        a.ldp_off(25, 26, 64);
        a.ldp_off(27, 28, 80);
    }
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    if scheduler_graph_epoch {
        a.ldp_off(29, 30, 0);
        a.add_imm(31, 31, frame_size as u32);
    } else {
        a.ldp_post(29, 30, frame_size);
    }
    a.ret();
    a.bind(l_ret_throw);
    a.str_imm(20, 19, 16);
    a.movz(0, 0, 0);
    a.ldp_d_off(8, 9, dsave);
    a.ldp_d_off(10, 11, dsave + 16);
    a.ldp_d_off(12, 13, dsave + 32);
    a.ldp_d_off(14, 15, dsave + 48);
    if scheduler_frame {
        a.ldp_off(23, 24, 48);
        a.ldp_off(25, 26, 64);
        a.ldp_off(27, 28, 80);
    }
    a.ldp_off(21, 22, 32);
    a.ldp_off(19, 20, 16);
    if scheduler_graph_epoch {
        a.ldp_off(29, 30, 0);
        a.add_imm(31, 31, frame_size as u32);
    } else {
        a.ldp_post(29, 30, frame_size);
    }
    a.ret();

    // ---- direct-call teardown stub (only reachable from emitted direct sequences) ----
    a.bind(l_direct_finish);
    if direct_on {
        emit_direct_finish_stub(&mut a, ilayout, rc_ok && layout.rc_strong_off == 0);
    }

    let words = a.finish();
    // Debug: `LUMEN_JIT_CODEDUMP=<substr>` prints the finished code words (hex, one per line)
    // of chunks whose leading slot names contain the substring — round-trip them through
    // `clang -c` + `objdump -d` for a disassembly of exactly what runs. Any value also prints
    // a `[jit-map]` line per compiled chunk (runtime base + length), which joins a `sample`
    // profile's raw addresses to chunk-relative offsets.
    let codedump_pat = std::env::var("LUMEN_JIT_CODEDUMP").ok();
    if let Some(pat) = &codedump_pat {
        let head: Vec<&str> = chunk
            .jit_slot_names()
            .iter()
            .take(4)
            .map(|s| &**s)
            .collect();
        let name = head.join(",");
        if !pat.is_empty() && name.contains(pat.as_str()) {
            eprintln!("[jit-codedump] fn({name}) {} words", words.len());
            for w in &words {
                eprintln!("[jit-codedump] {w:08x}");
            }
        }
    }
    let len = words.len() * 4;
    unsafe {
        let mem = sys::alloc_exec(words.as_ptr() as *const u8, len);
        if mem.is_null() {
            return None;
        }
        if codedump_pat.is_some() {
            let head: Vec<&str> = chunk
                .jit_slot_names()
                .iter()
                .take(4)
                .map(|s| &**s)
                .collect();
            eprintln!(
                "[jit-map] fn({}) base={:#x} len={len}",
                head.join(","),
                mem as usize
            );
        }
        if std::env::var_os("LUMEN_JIT_MAP").is_some() {
            let head: Vec<&str> = chunk
                .jit_slot_names()
                .iter()
                .take(4)
                .map(|s| &**s)
                .collect();
            let name = head.join("|");
            eprintln!(
                "[jit-map-range] {:x} {:x} {name}",
                mem as usize, len
            );
            for (pc, (&insn, op)) in pc_insn.iter().zip(ops).enumerate() {
                eprintln!(
                    "[jit-map-pc] {:x} {:x} {pc} {op:?} {name}",
                    mem as usize,
                    insn * 4
                );
            }
        }
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

#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[path = "jit_x64.rs"]
mod x64;

#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub fn compile(
    chunk: &Chunk,
    layout: &crate::value::JitLayout,
    ilayout: &crate::interpreter::InterpLayout,
) -> Option<JitCode> {
    x64::compile(chunk, layout, ilayout)
}

#[cfg(not(any(
    all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ),
    all(
        target_arch = "x86_64",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    )
)))]
pub fn compile(
    _chunk: &Chunk,
    _layout: &crate::value::JitLayout,
    _ilayout: &crate::interpreter::InterpLayout,
) -> Option<JitCode> {
    None
}

/// Whether `layout` is usable for the inline GetProp template: valid (probed std layouts hold)
/// and every offset it bakes fits its instruction's immediate range.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn get_prop_inlinable(layout: &crate::value::JitLayout) -> bool {
    let sh = layout.obj_props + layout.props_shape;
    let en = layout.obj_props + layout.props_entries + layout.vec_ptr_off;
    let enl = layout.obj_props + layout.props_entries + layout.vec_len_off;
    layout.valid
        // Packed property values are eight bytes. Until these templates decode them, keep only
        // property/name/element operations on their checked paths; unrelated JIT templates stay
        // enabled. In the old wide layout `meta` followed the full 16-byte Value.
        && layout.entry_accessor >= layout.entry_value + 8
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

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn guard_prop_data(a: &mut asm::Asm, reg: u32, base: u32, flags: u32, slow: usize) {
    a.ldrb_imm(reg, base, flags);
    let bit = asm::logical_imm_w(crate::value::PROP_ACCESSOR as u32).unwrap();
    a.logic_imm_w(0, reg, reg, bit);
    a.cbnz(reg, false, slow);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn guard_prop_writable(a: &mut asm::Asm, reg: u32, base: u32, flags: u32, slow: usize) {
    a.ldrb_imm(reg, base, flags);
    let bit = asm::logical_imm_w(crate::value::PROP_WRITABLE as u32).unwrap();
    a.logic_imm_w(0, reg, reg, bit);
    a.cbz(reg, false, slow);
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
/// Where a property read's receiver comes from.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy, PartialEq)]
enum PropRecv {
    /// Operand stack top (classic GetProp/GetMethod): consumed, refcount-managed.
    Stack,
    /// The frame's `this` binding (`ctx.this_val`): owned by the frame, no refcounting.
    This,
    /// A local slot (alive for the whole frame): no refcounting.
    Slot(u32),
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_prop_load_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    // Interp field offsets: string-primitive method receivers resolve against the ACTIVE
    // realm's String.prototype through ctx.interp.
    il: &crate::interpreter::InterpLayout,
    cache_ptr: usize,
    preferred: Option<crate::bytecode::IcState>,
    // The site's interned name (`chunk.jit_name(n)`): its data pointer keys the stub-cache
    // arm, and its bytes are the key-checked arm's compare immediates. Pinned by the chunk
    // the emitted code belongs to.
    name: &str,
    pc: u32,
    l_unwind: usize,
    method: bool,
    // Whether an `Exotic::Array` receiver may shape-validate: true when the site's (compile-time)
    // name cannot be an element key — element inserts don't transition an array's shape, but
    // element keys are all canonical indices, so a name that doesn't start with a digit cannot
    // collide with one. Prototype hops stay `Exotic::None`-only.
    arr_ok: bool,
    recv: PropRecv,
) {
    use crate::bytecode::{
        IC_OFF_DEPTH, IC_OFF_HOLDER_SHAPE, IC_OFF_MID_OK, IC_OFF_MID_SHAPE, IC_OFF_MID2_SHAPE,
        IC_OFF_RECV_SHAPE, IC_OFF_SLOT,
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
    // Key-checked entries (`IC_ARR_KEYCHK`: the holder is an Array — `arr.length`, or a method
    // on the Array-exotic `Array.prototype`): validated by an inline byte-compare of the entry's
    // key against the site's compile-time name. Only for short names with a good fat-pointer
    // probe; otherwise those states keep falling to the helper.
    let kc = layout.key_probe_ok && !name.is_empty() && name.len() <= 8;
    let slow = a.new_label();
    let done = a.new_label();
    let load = a.new_label();
    let load_kc = a.new_label();
    let absent_hit = a.new_label();
    // 1. receiver must be an Obj (tag 8); x10 = its stored Rc pointer, kept live for the final
    //    receiver drop (stack form) — hop walking uses x17. The this/slot forms read a binding
    //    the frame owns: no refcount management at all.
    // String-primitive receivers (method loads only — the receiver slot is never dropped or
    // refcounted on this path): resolve against the active realm's String.prototype, whose
    // stored Rc pointer plays the receiver role for the probe. The Rust helper fills the SAME
    // site cache with proto-based states (see get_prop_ic's primitive arm), so shapes align.
    let str_ok = method
        && il.valid
        && il.string_proto % 8 == 0
        && il.string_proto / 8 < 4096
        && name != "length"
        && name != "description"
        && !name.as_bytes().first().is_some_and(|b| b.is_ascii_digit());
    match recv {
        PropRecv::Stack => {
            a.ldurb(9, 20, -16);
            if str_ok {
                let obj_recv = a.new_label();
                let probe_go = a.new_label();
                a.cmp_imm_w(9, 8);
                a.b_cond(C_EQ, obj_recv);
                a.cmp_imm_w(9, 6);
                a.b_cond(C_NE, slow);
                a.ldr_imm(10, 19, 72); // ctx.interp
                a.ldr_imm(10, 10, il.string_proto as u32);
                a.b(probe_go);
                a.bind(obj_recv);
                a.ldur(10, 20, -8);
                a.bind(probe_go);
            } else {
                a.cmp_imm_w(9, 8);
                a.b_cond(C_NE, slow);
                a.ldur(10, 20, -8);
            }
            if !method {
                // receiver refcount > 1 (so the pop-drop below never frees)
                a.ldur(9, 10, strong);
                a.cmp_imm_x(9, 1);
                a.b_cond(C_LS, slow);
            }
        }
        PropRecv::This => {
            a.ldr_imm(14, 19, 48); // ctx.this_raw → the frame's `this` Value
            a.ldurb(9, 14, 0);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, slow);
            a.ldur(10, 14, 8);
        }
        PropRecv::Slot(off) => {
            a.ldrb_imm(9, 22, off);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, slow);
            a.ldr_imm(10, 22, off + 8);
        }
    }
    // After bytecode warmup, ordinary OO sites overwhelmingly have one stable depth/shape/slot.
    // Bake that state into a compact probe instead of embedding the full four-way/deep/exotic
    // state machine at every site. Every fact that made the cached slot authoritative is guarded
    // live; a miss goes to the checked helper, which observes mutations and arbitrary alternate
    // shapes exactly like the generic JIT miss path.
    let compact = preferred.filter(|st| st.depth <= 2 && (st.depth < 2 || st.mid_ok & 1 != 0));
    if let Some(st) = compact {
        a.add_imm(11, 10, rcv);
        a.ldrb_imm(14, 11, ex);
        a.cmp_imm_w(14, none_tag);
        a.b_cond(C_NE, slow);
        a.ldrb_imm(14, 11, plain);
        a.cbz(14, false, slow);
        a.ldr_w_imm(14, 11, sh);
        a.mov_imm64(16, st.recv_shape as u64);
        a.cmp_reg_w(14, 16);
        a.b_cond(C_NE, slow);
        if st.depth >= 1 {
            a.ldr_imm(17, 11, pr);
            a.cbz(17, true, slow);
            a.add_imm(11, 17, rcv);
            a.ldrb_imm(14, 11, ex);
            a.cmp_imm_w(14, none_tag);
            a.b_cond(C_NE, slow);
            a.ldrb_imm(14, 11, plain);
            a.cbz(14, false, slow);
            a.ldr_w_imm(14, 11, sh);
            let expected = if st.depth == 1 {
                st.holder_shape
            } else {
                st.mid_shape
            };
            a.mov_imm64(16, expected as u64);
            a.cmp_reg_w(14, 16);
            a.b_cond(C_NE, slow);
        }
        if st.depth == 2 {
            a.ldr_imm(17, 11, pr);
            a.cbz(17, true, slow);
            a.add_imm(11, 17, rcv);
            a.ldrb_imm(14, 11, ex);
            a.cmp_imm_w(14, none_tag);
            a.b_cond(C_NE, slow);
            a.ldrb_imm(14, 11, plain);
            a.cbz(14, false, slow);
            a.ldr_w_imm(14, 11, sh);
            a.mov_imm64(16, st.holder_shape as u64);
            a.cmp_reg_w(14, 16);
            a.b_cond(C_NE, slow);
        }
        a.mov_imm64(13, st.slot as u64);
        a.b(load);
    }
    // 2-5. probe every cache way (sites allocate PROP_IC_WAYS consecutive cells; the fill path
    // demotes ways one step, so a site rotating through up to that many shapes stabilizes with
    // one shape per way). Each probe is self-contained: it recomputes the receiver base from
    // x10 and jumps to `load` with x11 = holder base, x13 = slot.
    // `cache_ptr` = the IcState cell address, or 0 for "x12 already holds it" (the stub-cache
    // arm computes the entry address at run time).
    let probe = |a: &mut asm::Asm, cache_ptr: usize, miss: usize| {
        let d1 = a.new_label();
        if cache_ptr != 0 {
            a.mov_imm64(12, cache_ptr as u64);
        }
        a.ldrb_imm(9, 12, IC_OFF_DEPTH);
        a.ldr_w_imm(13, 12, IC_OFF_SLOT);
        // receiver hop: exotic None (or Array when `arr_ok` — but only as a NON-holder, so an
        // Array receiver additionally requires depth ≥ 1: its shape proves named-key ABSENCE,
        // not slot positions, because element entries occupy slots without transitioning the
        // shape; or StrWrap when `str_ok` — String.prototype/string wrappers intercept only
        // index and `length` reads, both excluded by the str_ok name gates), plain,
        // shape == recv_shape; x11 = receiver object base
        a.add_imm(11, 10, rcv);
        a.ldrb_imm(14, 11, ex);
        if arr_ok || str_ok {
            let ex_ok = a.new_label();
            a.cmp_imm_w(14, none_tag);
            a.b_cond(C_EQ, ex_ok);
            if arr_ok {
                let not_arr = a.new_label();
                a.cmp_imm_w(14, layout.exotic_array_tag as u32);
                a.b_cond(C_NE, not_arr);
                a.cbz(9, false, miss); // Array receiver must not be the holder (w9 = depth)
                a.b(ex_ok);
                a.bind(not_arr);
            }
            if str_ok {
                a.cmp_imm_w(14, layout.exotic_strwrap_tag as u32);
                a.b_cond(C_EQ, ex_ok);
            }
            a.b(miss);
            a.bind(ex_ok);
        } else {
            a.cmp_imm_w(14, none_tag);
            a.b_cond(C_NE, miss);
        }
        a.ldrb_imm(14, 11, plain);
        a.cbz(14, false, miss);
        a.ldr_w_imm(14, 11, sh);
        a.ldr_w_imm(16, 12, IC_OFF_RECV_SHAPE);
        a.cmp_reg_w(14, 16);
        a.b_cond(C_NE, miss);
        // depth routing: 0 → holder is the receiver; 1 → one hop; 2 → mid hop then fall
        // to d1. Non-plain depths divert to the key-checked decoder (`kc_route`) so the
        // common depths pay nothing for its existence.
        a.cbz(9, false, load);
        a.cmp_imm_w(9, 1);
        a.b_cond(C_EQ, d1);
        let kc_route = if kc { a.new_label() } else { miss };
        let other = a.new_label();
        a.cmp_imm_w(9, 2);
        a.b_cond(C_NE, other);
        a.ldrb_imm(14, 12, IC_OFF_MID_OK);
        a.cbz(14, false, miss);
        // depth-2 mid hop: follow the live proto, validate against mid_shape
        a.ldr_imm(17, 11, pr); // Option<Gc> niche: pointer or 0
        a.cbz(17, true, miss);
        a.add_imm(11, 17, rcv);
        a.ldrb_imm(14, 11, ex);
        a.cmp_imm_w(14, none_tag);
        a.b_cond(C_NE, miss);
        a.ldrb_imm(14, 11, plain);
        a.cbz(14, false, miss);
        a.ldr_w_imm(14, 11, sh);
        a.ldr_w_imm(16, 12, IC_OFF_MID_SHAPE);
        a.cmp_reg_w(14, 16);
        a.b_cond(C_NE, miss);
        // holder hop (depth 1 entry point; depth 2 falls through): validate holder_shape
        a.bind(d1);
        a.ldr_imm(17, 11, pr);
        a.cbz(17, true, miss);
        a.add_imm(11, 17, rcv);
        a.ldrb_imm(14, 11, ex);
        a.cmp_imm_w(14, none_tag);
        a.b_cond(C_NE, miss);
        a.ldrb_imm(14, 11, plain);
        a.cbz(14, false, miss);
        a.ldr_w_imm(14, 11, sh);
        a.ldr_w_imm(16, 12, IC_OFF_HOLDER_SHAPE);
        a.cmp_reg_w(14, 16);
        a.b_cond(C_NE, miss);
        a.b(load);
        // Cached ABSENCE (`IC_ABSENT`, the AST-shaped read `node.optionalField`): re-walk the
        // live chain — every level None-exotic, ic-plain, shape matching the recorded walk
        // (level 1 already validated by the receiver checks above; ABSENT states only fill
        // from all-None chains, so re-require None on receivers the `arr_ok`/`str_ok` gates
        // let through) — and the chain must END where the fill saw it end. Then the read is
        // `undefined` with no entry scan at all. Method loads keep the helper (an absent
        // method throws there anyway).
        a.bind(other);
        if !method {
            a.cmp_imm_w(9, crate::bytecode::IC_ABSENT as u32);
            a.b_cond(C_NE, kc_route);
            if arr_ok || str_ok {
                a.ldrb_imm(14, 11, ex);
                a.cmp_imm_w(14, none_tag);
                a.b_cond(C_NE, miss);
            }
            let chain_end = a.new_label();
            for (lvl, shape_off) in [
                (2u32, IC_OFF_MID_SHAPE),
                (3u32, IC_OFF_MID2_SHAPE),
                (4u32, IC_OFF_HOLDER_SHAPE),
            ] {
                a.cmp_imm_w(13, lvl);
                a.b_cond(C_LO, chain_end);
                a.ldr_imm(17, 11, pr);
                a.cbz(17, true, miss); // chain ended before the recorded level count
                a.add_imm(11, 17, rcv);
                a.ldrb_imm(14, 11, ex);
                a.cmp_imm_w(14, none_tag);
                a.b_cond(C_NE, miss);
                a.ldrb_imm(14, 11, plain);
                a.cbz(14, false, miss);
                a.ldr_w_imm(14, 11, sh);
                a.ldr_w_imm(16, 12, shape_off);
                a.cmp_reg_w(14, 16);
                a.b_cond(C_NE, miss);
            }
            a.bind(chain_end);
            a.ldr_imm(17, 11, pr);
            a.cbz(17, true, absent_hit);
            a.b(miss); // a proto was attached where the fill saw the end
        } else if !kc {
            a.b(miss);
        }
        // key-checked states (`IC_ARR_KEYCHK`): 0x40 = the array receiver IS the holder
        // (`arr.length`); 0x41 = one hop to an array holder (Array.prototype methods — itself
        // an Array exotic). The receiver-side Array gate above already passed 0x40 (nonzero
        // depth). Deeper key-checked states → helper.
        if kc {
            a.bind(kc_route);
            a.cmp_imm_w(9, 0x40);
            a.b_cond(C_EQ, load_kc);
            a.cmp_imm_w(9, 0x41);
            a.b_cond(C_NE, miss);
            // one proto hop; the holder may be Exotic::None or an Array (its entry key gets
            // re-checked, which is what makes an array holder's slot trustworthy at all)
            a.ldr_imm(17, 11, pr);
            a.cbz(17, true, miss);
            a.add_imm(11, 17, rcv);
            a.ldrb_imm(14, 11, ex);
            let ex_ok = a.new_label();
            a.cmp_imm_w(14, none_tag);
            a.b_cond(C_EQ, ex_ok);
            a.cmp_imm_w(14, layout.exotic_array_tag as u32);
            a.b_cond(C_NE, miss);
            a.bind(ex_ok);
            a.ldrb_imm(14, 11, plain);
            a.cbz(14, false, miss);
            a.ldr_w_imm(14, 11, sh);
            a.ldr_w_imm(16, 12, IC_OFF_HOLDER_SHAPE);
            a.cmp_reg_w(14, 16);
            a.b_cond(C_NE, miss);
            a.b(load_kc);
        }
    };
    // Loop the self-contained probe over every way (x8 = way cursor, w7 = ways left — both
    // untouched by the probe body; a hit exits through `load`/`load_kc`). One emitted body
    // instead of PROP_IC_WAYS unrolled copies keeps per-site code size flat.
    if compact.is_none() {
        let l_way = a.new_label();
        let l_way_next = a.new_label();
        a.mov_imm64(8, cache_ptr as u64);
        a.movz(7, crate::bytecode::PROP_IC_WAYS as u32, 0);
        a.bind(l_way);
        a.mov(12, 8);
        probe(a, 0, l_way_next);
        a.bind(l_way_next);
        let ic_stride = std::mem::size_of::<std::cell::Cell<crate::bytecode::IcState>>();
        a.add_imm(8, 8, ic_stride as u32);
        a.sub_imm(7, 7, 1);
        a.cbnz(7, false, l_way);
        a.b(slow);
    }
    // 6. x11 = holder base: bounds-check the cached slot against the live entries length
    //    (defense in depth — fills only record exact-slot holders, but an OOB read through a
    //    stale cache would be memory-unsafe, so verify), then entry = entries + slot*size;
    //    data property; non-BigInt
    let val = a.new_label();
    a.bind(load);
    a.ldr_imm(16, 11, en_len);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    a.bind(val);
    guard_prop_data(a, 9, 15, ea, slow);
    if layout.entry_accessor == layout.entry_value + 8 {
        // Decode the NaN-box into the execution tier's wide `{tag,payload}` pair. BigInt keeps
        // the checked path (matching the old template); strings/symbols/objects clone by bumping
        // the strong count at their untagged pointer.
        a.ldur(13, 15, ev);
        a.lsr_imm(9, 13, 48); // packed tag prefix
        let decoded = a.new_label();
        let is_undefined = a.new_label();
        let is_empty = a.new_label();
        let is_null = a.new_label();
        let is_bool = a.new_label();
        let is_str = a.new_label();
        let is_sym = a.new_label();
        let is_obj = a.new_label();
        let is_number = a.new_label();
        // Objects and Numbers dominate hot reads. Test the negative-tag Object first, then the
        // contiguous positive-tag range; ordinary positive/negative f64 prefixes take the
        // three-branch Number path instead of walking every tag.
        a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_EQ, is_obj);
        a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_LO, is_number);
        a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_HI, is_number);
        for (tag, label) in [
            (crate::value::PACK_BOOL, is_bool),
            (crate::value::PACK_STR, is_str),
            (crate::value::PACK_SYM, is_sym),
            (crate::value::PACK_BIGINT, slow),
            (crate::value::PACK_UNDEFINED, is_undefined),
            (crate::value::PACK_EMPTY, is_empty),
            (crate::value::PACK_NULL, is_null),
        ] {
            a.movz(16, (tag >> 48) as u32, 0);
            a.cmp_reg_x(9, 16);
            a.b_cond(C_EQ, label);
        }
        a.bind(is_number);
        a.movz(12, 4, 0);
        a.b(decoded);
        for (label, tag) in [(is_undefined, 0), (is_empty, 1), (is_null, 2)] {
            a.bind(label);
            a.movz(12, tag, 0);
            a.movz(13, 0, 0);
            a.b(decoded);
        }
        a.bind(is_bool);
        a.movz(12, 3, 0);
        // `repr(u8) Value::Bool` keeps its bool payload at byte 1 of the tag word.
        a.lsl_imm_w(13, 13, 8);
        a.logic_w(1, 12, 12, 13);
        a.movz(13, 0, 0);
        a.b(decoded);
        for (label, tag) in [(is_str, 6), (is_sym, 7), (is_obj, 8)] {
            a.bind(label);
            a.movz(12, tag, 0);
            a.lsl_imm(13, 13, 16);
            a.lsr_imm(13, 13, 16);
            a.ldur(16, 13, strong);
            a.add_imm(16, 16, 1);
            a.stur(16, 13, strong);
            a.b(decoded);
        }
        a.bind(decoded);
    } else {
        a.ldurb(9, 15, ev); // w9 = value tag (kept live through the loads below)
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.ldur(12, 15, ev);
        a.ldur(13, 15, ev + 8); // payload word (the Rc pointer for tags 6..8)
        let nobump = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nobump);
        a.ldur(16, 13, strong);
        a.add_imm(16, 16, 1);
        a.stur(16, 13, strong);
        a.bind(nobump);
    }
    // --- commit: everything validated; from here only writes ---
    if !matches!(recv, PropRecv::Stack) {
        // this/slot receivers were never on the stack: just push the value.
        a.stur(12, 20, 0);
        a.stur(13, 20, 8);
        a.add_imm(20, 20, 16);
    } else if method {
        // receiver stays at [-16]; push the method above it
        a.stur(12, 20, 0);
        a.stur(13, 20, 8);
        a.add_imm(20, 20, 16);
    } else {
        // drop the receiver (strong was > 1: decrement, no free). If the value IS the receiver
        // the bump above already balanced this (the count is re-read).
        a.ldur(9, 10, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 10, strong);
        // overwrite the receiver slot with the value (pop obj + push value = same depth)
        a.stur(12, 20, -16);
        a.stur(13, 20, -8);
    }
    a.b(done);
    // 6kc. Key-checked landing (out of the hit path's fall-through line): same bounds + entry
    // compute as `load`, then verify the entry's key IS the site's name (length, then content
    // against immediates) — an array's slots aren't pinned by its shape, so the key is the
    // authority. Mismatch (slot shifted since fill) → helper re-derives. Ends by jumping back
    // into the shared value path.
    a.bind(load_kc);
    if kc {
        a.ldr_imm(16, 11, en_len);
        a.cmp_reg_x(13, 16);
        a.b_cond(C_HS, slow);
        a.ldr_imm(15, 11, en);
        a.mov_imm64(16, es);
        a.madd(15, 13, 16, 15);
        let klen = (layout.entry_key + layout.str_len_word) as i32;
        let kptr = (layout.entry_key + layout.str_ptr_word) as i32;
        a.ldur(16, 15, klen);
        a.cmp_imm_x(16, name.len() as u32);
        a.b_cond(C_NE, slow);
        a.ldur(16, 15, kptr); // stored Rc<str> word (RcBox base)
        let d = layout.str_data_off as u32;
        let bytes = name.as_bytes();
        let mut off = 0usize;
        while bytes.len() - off >= 4 {
            let imm =
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            a.ldr_w_imm(17, 16, d + off as u32);
            a.movz(9, imm & 0xFFFF, 0);
            a.movk(9, imm >> 16, 1);
            a.cmp_reg_w(17, 9);
            a.b_cond(C_NE, slow);
            off += 4;
        }
        if bytes.len() - off >= 2 {
            let imm = u16::from_le_bytes([bytes[off], bytes[off + 1]]) as u32;
            a.ldrh_imm(17, 16, d + off as u32);
            a.movz(9, imm, 0);
            a.cmp_reg_w(17, 9);
            a.b_cond(C_NE, slow);
            off += 2;
        }
        if bytes.len() - off == 1 {
            a.ldrb_imm(17, 16, d + off as u32);
            a.cmp_imm_w(17, bytes[off] as u32);
            a.b_cond(C_NE, slow);
        }
        a.b(val);
    }
    // 6a. absent landing: the read is `undefined`. Tag-only write (stale payload is fine —
    // Undefined drops touch nothing; the Tdz template sets the same precedent).
    a.bind(absent_hit);
    if !method {
        a.movz(9, 0, 0);
        match recv {
            PropRecv::Stack => {
                // drop the receiver (strong was > 1: decrement, no free), overwrite in place
                a.ldur(14, 10, strong);
                a.sub_imm(14, 14, 1);
                a.stur(14, 10, strong);
                a.sturb(9, 20, -16);
            }
            PropRecv::This | PropRecv::Slot(_) => {
                a.strb_imm(9, 20, 0);
                a.add_imm(20, 20, 16);
            }
        }
        a.b(done);
    }
    a.bind(slow);
    emit_op_helper(a, H_GET_PROP, pc, l_unwind);
    a.bind(done);
}

/// The direct (shared-ctx) JIT→JIT call sequence, emitted after the way-1 probe hit when the
/// fill-time gates allow it (see [`crate::bytecode::CallIc::direct`]). Everything the layered
/// path does survives — recursion depth, the amortized gc tick (a due tick falls to the
/// generic path BEFORE any mutation), the `FnFrame`, constructing/new.target clearing, the
/// callee's own handler watermark — but the callee runs on the CALLER's `JitCtx` with its
/// frame fields swapped, entered by a bare `blr`: no helper dispatch, no probe re-read, no
/// fresh JitCtx, no `run_moved`. Teardown (drops, pool return, frame pop, tail drain) is one
/// `H_DIRECT_FINISH` call; the sequence then restores every swapped field and either pushes
/// the return value or routes to the caller's unwind. Falls back to `hit_slow` (the
/// H_CALL_HIT path) on any gate failure, with NO state mutated.
///
/// Returns false (nothing emitted) when an emission-time precondition fails — the caller then
/// emits only the probe + H_CALL_HIT form.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_direct_call(
    a: &mut asm::Asm,
    ilayout: &crate::interpreter::InterpLayout,
    // Stored-pointer → `Rc::as_ptr` header delta (`JitLayout::gc_data_off`): FnFrame.fn_ptr
    // records the as_ptr identity (FnFrame::callee reconstructs an Rc from it).
    gc_data_off: usize,
    // Byte offset of `Chunk::inline_attempted` (computed by the caller from its own chunk —
    // same monomorphized layout as the callee's): the sequence requires the callee's one-shot
    // recompile to have happened (or been attempted), else it keeps taking H_CALL_HIT, which
    // is what bumps jit_runs toward the trigger. A landed code2 bumps the epoch and refills
    // the site with the new chunk anyway.
    attempted_off: usize,
    argc: usize,
    with_this: bool,
    hit_slow: usize,
    gc_slow: usize,
    l_unwind: usize,
    done: usize,
    // Label of the chunk's shared direct-finish stub (`emit_direct_finish_stub`).
    finish_stub: usize,
) -> bool {
    use std::mem::offset_of;
    // Emission gates: probed Interp offsets must exist and fit the addressing modes used. Wide
    // calls address their operand block through a computed positive-offset base, rather than
    // signed LDUR offsets from sp (which capped the old sequence at eight arguments). Keep a
    // generous code-size ceiling: argument moves are unrolled and real-world JS call sites are
    // overwhelmingly below 64.
    if !ilayout.valid || argc > 64 || gc_data_off >= 4096 {
        return false;
    }
    let fits8 = |o: usize| o % 8 == 0 && o / 8 < 4096;
    let fits4 = |o: usize| o % 4 == 0 && o / 4 < 4096;
    let il = ilayout;
    if !(fits4(il.depth)
        && fits4(il.gc_tick)
        && fits8(il.gc_next)
        && fits4(il.cur_coro)
        && il.constructing < 4096
        && fits8(il.new_target)
        && fits8(il.fn_frames + il.fnf_ptr_word)
        && fits8(il.fn_frames + il.fnf_len_word)
        && fits8(il.fn_frames + il.fnf_cap_word)
        && fits8(il.frame_pool + il.fp_ptr_word)
        && fits8(il.frame_pool + il.fp_len_word)
        && fits8(il.new_target + 8))
    {
        return false;
    }
    // The handlers Vec's length-word offset within JitCtx (per-instantiation, probed here).
    let handlers_len_off = {
        let mut v: Vec<(u32, usize)> = Vec::with_capacity(5);
        v.push((1, 1));
        v.push((2, 2));
        v.push((3, 3));
        let words: [usize; 3] =
            unsafe { std::mem::transmute_copy::<Vec<(u32, usize)>, [usize; 3]>(&v) };
        let Some(w) = words.iter().position(|w| *w == 3) else {
            return false;
        };
        offset_of!(JitCtx, handlers) + w * 8
    };
    if !fits8(handlers_len_off) {
        return false;
    }
    const IC_ENV: i32 = 8;
    const IC_STRICT: u32 = 40;
    const IC_USES_THIS: u32 = 41;
    const IC_NPARAMS: u32 = 42;
    const IC_NSLOTS: u32 = 44;
    const IC_DIRECT: i32 = 46;
    const IC_CHUNK_RAW: i32 = 64;
    const IC_CODE_MEM: i32 = 72;
    const IC_PC_OFFS: i32 = 80;
    let cx_slots = offset_of!(JitCtx, slots) as u32;
    let cx_stack_base = offset_of!(JitCtx, stack_base) as u32;
    let cx_env_raw = offset_of!(JitCtx, env_raw) as u32;
    let cx_chunk = offset_of!(JitCtx, chunk) as u32;
    let cx_n_slots = offset_of!(JitCtx, n_slots) as u32;
    let cx_code_base = offset_of!(JitCtx, code_base) as u32;
    let cx_pc_offsets = offset_of!(JitCtx, pc_offsets) as u32;
    let cx_floor = offset_of!(JitCtx, handler_floor) as u32;
    let cx_this = offset_of!(JitCtx, this_val) as u32;
    let cx_ret = offset_of!(JitCtx, ret) as u32;
    if !(fits8(cx_this as usize) && fits8(cx_ret as usize)) {
        return false;
    }
    // rc strong at payload+0 — same contract as the templates (layout.valid checked upstream).
    let strong = 0i32;

    // ---- checks (entry state: x12 = ic0 ptr, x10 = callee stored Rc ptr; NO mutations) ----
    // direct bits 0 (no force resets) and 2 (recompile settled)
    if attempted_off >= 4096 {
        return false;
    }
    a.ldurb(9, 12, IC_DIRECT);
    let field1b = asm::logical_imm_w(1).unwrap();
    a.logic_imm_w(0, 11, 9, field1b); // bit 0: no force resets
    a.cbz(11, false, hit_slow);
    let field8 = asm::logical_imm_w(8).unwrap();
    a.logic_imm_w(0, 11, 9, field8); // bit 3: frame fits FRAME_BUF
    a.cbz(11, false, hit_slow);
    // recompile settled (live chunk byte — see attempted_off)
    a.ldur(11, 12, IC_CHUNK_RAW);
    a.ldrb_imm(11, 11, attempted_off as u32);
    a.cbz(11, false, hit_slow);
    // needs_global (bit 1) requires a live ctx.global_body
    let no_glob = a.new_label();
    let field2 = asm::logical_imm_w(2).unwrap();
    a.logic_imm_w(0, 11, 9, field2);
    a.cbz(11, false, no_glob);
    a.ldr_imm(11, 19, 56); // ctx.global_body
    a.cbz(11, true, hit_slow);
    a.bind(no_glob);
    // n_params >= argc: the sequence moves exactly `argc` arguments and its slot-init loop
    // already tags every remaining slot (argc..n_slots) Undefined, which IS the missing-
    // argument binding. Over-application (argc > n_params) keeps the helper: the surplus
    // values must be dropped, and refcounted drops don't belong in this sequence.
    a.ldrh_imm(9, 12, IC_NPARAMS);
    a.cmp_imm_w(9, argc as u32);
    a.b_cond(C_LO, hit_slow);
    if PACKED_LOCAL_SLOTS {
        // BigInt owns compound storage and is intentionally not NaN-boxed. Reject it before
        // any direct-call state mutation; the layered path handles the move generically.
        for k in 0..argc {
            a.ldurb(9, 20, -((argc - k) as i32 * 16));
            a.cmp_imm_w(9, 5);
            a.b_cond(C_EQ, hit_slow);
        }
    }
    // this binding: a this-using SLOPPY callee needs boxing/global fallback unless the
    // incoming receiver is already an object.
    a.ldrb_imm(9, 12, IC_USES_THIS);
    let this_ok = a.new_label();
    a.cbz(9, false, this_ok);
    a.ldrb_imm(9, 12, IC_STRICT);
    a.cbnz(9, false, this_ok);
    if with_this {
        a.sub_imm(9, 20, ((argc + 2) * 16) as u32);
        a.ldrb_imm(9, 9, 0);
        a.cmp_imm_w(9, 8);
        a.b_cond(C_NE, hit_slow);
    } else {
        a.b(hit_slow); // no receiver + sloppy this-user: global boxing → generic
    }
    a.bind(this_ok);
    // callee refcount > 1 (the post-call dec then never frees mid-sequence; a same-call
    // binding deletion is caught by the post-dec zero check)
    a.ldur(9, 10, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, hit_slow);
    // interp-side room: depth, gc tick, fn_frames capacity, frame pool
    a.ldr_imm(14, 19, 72); // ctx.interp
    a.ldr_w_imm(11, 14, il.depth as u32);
    a.mov_imm64(13, crate::interpreter::MAX_EVAL_DEPTH as u64);
    a.cmp_reg_x(11, 13); // w-load zero-extends; the compare stays 64-bit
    a.b_cond(C_HS, hit_slow);
    // Check actual allocation pressure in generated code. Collection-due calls take the full
    // cache-reprobing helper because a collection can invalidate validated raw call state.
    a.mov_imm64(4, crate::value::live_objects_ptr() as u64);
    a.ldr_imm(4, 4, 0);
    a.ldr_imm(13, 14, il.gc_next as u32);
    let gc_due = a.new_label();
    a.cmp_reg_x(4, 13);
    a.b_cond(C_GT, gc_due);
    a.ldr_w_imm(13, 14, il.gc_tick as u32);
    a.add_imm(13, 13, 1);
    let maint_mask = asm::logical_imm_w(crate::interpreter::GC_DIRECT_MAINT_MASK).unwrap();
    a.logic_imm_w(0, 4, 13, maint_mask);
    a.cbz(4, false, hit_slow);
    a.ldr_imm(16, 14, (il.fn_frames + il.fnf_len_word) as u32);
    a.ldr_imm(17, 14, (il.fn_frames + il.fnf_cap_word) as u32);
    a.cmp_reg_x(16, 17);
    a.b_cond(C_HS, hit_slow);
    a.ldr_imm(7, 14, (il.frame_pool + il.fp_len_word) as u32);
    a.cbz(7, true, hit_slow);

    // ---- mutations ----
    a.add_imm(11, 11, 1);
    a.str_w_imm(11, 14, il.depth as u32); // depth++ (u32 field)
    a.str_w_imm(13, 14, il.gc_tick as u32); // tick (not due)
    // FnFrame push: entry = ptr + len*24
    a.ldr_imm(6, 14, (il.fn_frames + il.fnf_ptr_word) as u32);
    a.movz(5, 24, 0);
    a.madd(6, 16, 5, 6);
    a.add_imm(4, 10, gc_data_off as u32);
    a.stur(4, 6, 0); // fn_ptr = the callee's as_ptr identity
    a.ldr_w_imm(5, 14, il.cur_coro as u32);
    a.str_w_imm(5, 6, 8); // coro
    a.ldrb_imm(5, 12, IC_STRICT);
    a.sturb(5, 6, 12); // strict
    a.stur(31, 6, 16); // extra = None (xzr)
    a.add_imm(16, 16, 1);
    a.str_imm(16, 14, (il.fn_frames + il.fnf_len_word) as u32);
    // frame pool pop: buf = ptr[--len] → x9 (the callee slots base)
    a.sub_imm(7, 7, 1);
    a.str_imm(7, 14, (il.frame_pool + il.fp_len_word) as u32);
    a.ldr_imm(5, 14, (il.frame_pool + il.fp_ptr_word) as u32);
    a.ldr_x_lsl3(9, 5, 7);
    // Move the arguments off the wide caller operand stack. ARM64 callees consume packed local
    // words; x64 retains the established wide frame ABI.
    if PACKED_LOCAL_SLOTS {
        a.mov(8, 9); // the encoder uses x9 for the wide discriminant
        for k in 0..argc {
            let off = -((argc - k) as i32 * 16);
            emit_packed_stack_encode_all(a, off, hit_slow);
            a.str_imm(16, 8, (k * 8) as u32);
        }
        a.mov(9, 8);
    } else {
        let bytes = argc * 16;
        a.sub_imm(8, 20, bytes as u32);
        for k in 0..argc {
            a.ldr_imm(4, 8, (k * 16) as u32);
            a.ldr_imm(5, 8, (k * 16 + 8) as u32);
            a.str_imm(4, 9, (k * 16) as u32);
            a.str_imm(5, 9, (k * 16 + 8) as u32);
        }
    }
    // Initialize the remaining slots to Undefined.
    a.ldrh_imm(5, 12, IC_NSLOTS); // w5 = n_slots (stays live for stack_base below)
    a.movz(6, argc as u32, 0);
    a.movz(4, if PACKED_LOCAL_SLOTS { 8 } else { 16 }, 0);
    if PACKED_LOCAL_SLOTS {
        a.mov_imm64(17, crate::value::PACK_UNDEFINED);
    }
    let init_loop = a.new_label();
    let init_done = a.new_label();
    a.bind(init_loop);
    a.cmp_reg_w(6, 5);
    a.b_cond(C_HS, init_done);
    a.madd(3, 6, 4, 9);
    if PACKED_LOCAL_SLOTS {
        a.stur(17, 3, 0);
    } else {
        a.sturb(31, 3, 0);
    }
    a.add_imm(6, 6, 1);
    a.b(init_loop);
    a.bind(init_done);
    if PACKED_LOCAL_SLOTS {
        // The packed encoder uses x14 for tag prefixes; restore the Interp base used by the
        // remainder of the frame swap.
        a.ldr_imm(14, 19, 72);
    }

    // ---- swap: save the caller's frame fields to an SP-carved area, install the callee's --
    a.sub_imm(31, 31, 128);
    // slots
    a.ldr_imm(4, 19, cx_slots);
    a.stur(4, 31, 0);
    a.str_imm(9, 19, cx_slots);
    // stack_base = slots + n_slots*16
    a.ldr_imm(4, 19, cx_stack_base);
    a.stur(4, 31, 8);
    a.movz(4, 16, 0);
    a.madd(3, 5, 4, 9);
    a.str_imm(3, 19, cx_stack_base);
    // env_raw
    a.ldr_imm(4, 19, cx_env_raw);
    a.stur(4, 31, 16);
    a.ldur(4, 12, IC_ENV);
    a.str_imm(4, 19, cx_env_raw);
    // chunk
    a.ldr_imm(4, 19, cx_chunk);
    a.stur(4, 31, 24);
    a.ldur(4, 12, IC_CHUNK_RAW);
    a.str_imm(4, 19, cx_chunk);
    // n_slots
    a.ldr_imm(4, 19, cx_n_slots);
    a.stur(4, 31, 32);
    a.str_imm(5, 19, cx_n_slots);
    // code_base
    a.ldr_imm(4, 19, cx_code_base);
    a.stur(4, 31, 40);
    a.ldur(4, 12, IC_CODE_MEM);
    a.str_imm(4, 19, cx_code_base);
    // pc_offsets
    a.ldr_imm(4, 19, cx_pc_offsets);
    a.stur(4, 31, 48);
    a.ldur(4, 12, IC_PC_OFFS);
    a.str_imm(4, 19, cx_pc_offsets);
    // handler_floor = live handlers.len
    a.ldr_imm(4, 19, cx_floor);
    a.stur(4, 31, 56);
    a.ldr_imm(4, 19, handlers_len_off as u32);
    a.str_imm(4, 19, cx_floor);
    // this_val (16B): save old, install the callee's
    a.ldr_imm(4, 19, cx_this);
    a.ldr_imm(5, 19, cx_this + 8);
    a.stur(4, 31, 64);
    a.stur(5, 31, 72);
    if with_this {
        // ALWAYS move the receiver into ctx.this_val — even when the callee never reads
        // `this` — because the finish helper's `this_val = Undefined` is what consumes it
        // (skipping the caller-stack slot at cleanup without this move leaked the receiver
        // on every this-less method call; Splay OOM'd on exactly that).
        a.sub_imm(3, 20, ((argc + 2) * 16) as u32);
        a.ldr_imm(4, 3, 0);
        a.ldr_imm(5, 3, 8);
        a.str_imm(4, 19, cx_this);
        a.str_imm(5, 19, cx_this + 8);
    } else {
        a.strb_imm(31, 19, cx_this); // Undefined tag (payload stale; tag-only reads)
    }
    // constructing (byte) + new_target (tag byte): cleared for the callee
    a.ldrb_imm(4, 14, il.constructing as u32);
    a.stur(4, 31, 88);
    a.strb_imm(31, 14, il.constructing as u32);
    a.ldr_imm(4, 14, il.new_target as u32);
    a.ldr_imm(5, 14, (il.new_target + 8) as u32);
    a.stur(4, 31, 96);
    a.stur(5, 31, 104);
    a.strb_imm(31, 14, il.new_target as u32); // Undefined tag

    // ---- run the callee on the shared ctx ----
    a.mov(0, 19);
    a.ldur(16, 12, IC_CODE_MEM);
    a.blr(16);
    // w0 = 1 ok / 0 threw → w1 = threw for the finish stub
    let field1 = asm::logical_imm_w(1).unwrap();
    a.logic_imm_w(2, 1, 0, field1); // eor w1, w0, #1
    // Teardown (drops, pool return, frame pop, tail drain, depth--): one shared per-chunk stub
    // (see `emit_direct_finish_stub`) whose fast path never leaves machine code. w8 = threw.
    a.bl_label(finish_stub);

    // ---- restore every swapped field ----
    a.ldr_imm(14, 19, 72); // ctx.interp (x14 was clobbered by the callee/helpers)
    a.ldur(4, 31, 0);
    a.str_imm(4, 19, cx_slots);
    a.ldur(4, 31, 8);
    a.str_imm(4, 19, cx_stack_base);
    a.ldur(4, 31, 16);
    a.str_imm(4, 19, cx_env_raw);
    a.ldur(4, 31, 24);
    a.str_imm(4, 19, cx_chunk);
    a.ldur(4, 31, 32);
    a.str_imm(4, 19, cx_n_slots);
    a.ldur(4, 31, 40);
    a.str_imm(4, 19, cx_code_base);
    a.ldur(4, 31, 48);
    a.str_imm(4, 19, cx_pc_offsets);
    a.ldur(4, 31, 56);
    a.str_imm(4, 19, cx_floor);
    a.ldur(4, 31, 64);
    a.ldur(5, 31, 72);
    a.str_imm(4, 19, cx_this);
    a.str_imm(5, 19, cx_this + 8);
    a.ldur(4, 31, 88);
    a.strb_imm(4, 14, il.constructing as u32);
    a.ldur(4, 31, 96);
    a.ldur(5, 31, 104);
    a.str_imm(4, 14, il.new_target as u32);
    a.str_imm(5, 14, (il.new_target + 8) as u32);
    a.add_imm(31, 31, 128);

    // ---- pop the callee (and skip the consumed this slot); dispatch on threw ----
    a.sub_imm(3, 20, ((argc + 1) * 16) as u32);
    a.ldr_imm(5, 3, 8);
    a.ldur(6, 5, strong);
    a.sub_imm(6, 6, 1);
    a.stur(6, 5, strong);
    let no_free = a.new_label();
    a.cbnz(6, true, no_free);
    // last reference (binding deleted during the call): real drop via helper
    a.mov(0, 19);
    a.movz(1, 0, 0);
    a.mov_imm64(6, (argc as u64 + 1) * 16);
    a.sub_reg(2, 20, 6);
    a.ldr_imm(16, 21, (H_DROP_AT * 8) as u32);
    a.blr(16);
    a.bind(no_free);
    let popped = ((argc + 1 + with_this as usize) * 16) as u32;
    a.sub_imm(20, 20, popped);
    a.cbnz(8, true, l_unwind); // threw → caller unwind (fields restored)
    // push ctx.ret (move: reset its tag to Undefined)
    a.ldr_imm(4, 19, cx_ret);
    a.ldr_imm(5, 19, cx_ret + 8);
    a.stur(4, 20, 0);
    a.stur(5, 20, 8);
    a.strb_imm(31, 19, cx_ret);
    a.add_imm(20, 20, 16);
    a.b(done);
    a.bind(gc_due);
    a.movz(13, crate::interpreter::GC_CALL_POLL_MASK, 0);
    a.str_w_imm(13, 14, il.gc_tick as u32);
    a.b(gc_slow);
    true
}

/// The direct-call teardown stub, emitted ONCE per chunk (sites reach it by `bl`; per-site
/// inlining would grow every call site by ~90 instructions). Entry: w1 = threw, x19 = ctx
/// (still holding the CALLEE's swapped frame fields), x21 = helpers. Exit: w8 = final threw,
/// everything else caller-saved clobbered. The fast path replicates `jit_direct_finish` for
/// the common shape — clean return, empty operand stack, no pending tail call, no
/// materialized FnFrame extra, room in the frame pool, and every owned Value (slots +
/// `this`) either trivially droppable (tag < 5) or a shared reference (bare strong-count
/// decrement) — in two passes: validate everything with NO mutation, then commit. Any
/// deviation falls to the H_DIRECT_FINISH helper with state untouched.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_direct_finish_stub(
    a: &mut asm::Asm,
    il: &crate::interpreter::InterpLayout,
    // The templates' rc contract (strong at payload+0) — without it every teardown takes the
    // helper (which falls back to real drops).
    rc_dec_ok: bool,
) {
    use std::mem::offset_of;
    let cx_this = offset_of!(JitCtx, this_val) as u32;
    let cx_slots = offset_of!(JitCtx, slots) as u32;
    let cx_n_slots = offset_of!(JitCtx, n_slots) as u32;
    let slow = a.new_label();
    let fits8 = |o: usize| o % 8 == 0 && o / 8 < 4096;
    let fast_ok = rc_dec_ok
        && il.valid
        && fits8(cx_this as usize)
        && fits8(cx_slots as usize)
        && fits8(cx_n_slots as usize)
        && fits8(il.pending_tail)
        && fits8(il.fn_frames + il.fnf_ptr_word)
        && fits8(il.fn_frames + il.fnf_len_word)
        && fits8(il.frame_pool + il.fp_ptr_word)
        && fits8(il.frame_pool + il.fp_len_word)
        && fits8(il.frame_pool + il.fp_cap_word)
        && il.depth % 4 == 0
        && il.depth / 4 < 4096;
    // The stub calls out (H_DROP_AT per last-reference Value, or the full helper), so lr is
    // spilled for the whole body; both exits share the epilogue.
    let done = a.new_label();
    a.stp_pre(29, 30, -16);
    if fast_ok {
        a.cbnz(1, false, slow); // threw → helper
        a.ldr_imm(14, 19, 72); // ctx.interp
        // operand stack clean (a clean return always leaves final_sp == stack_base)
        a.ldr_imm(9, 19, 16); // ctx.final_sp
        a.ldr_imm(10, 19, 8); // ctx.stack_base
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, slow);
        // no pending proper-tail-call (Option<Box> niche: None = 0)
        a.ldr_imm(9, 14, il.pending_tail as u32);
        a.cbnz(9, true, slow);
        // FnFrame top: no materialized `extra` (the asm push wrote None; only the callee's own
        // arguments-object materialization could have filled it)
        a.ldr_imm(16, 14, (il.fn_frames + il.fnf_len_word) as u32);
        a.ldr_imm(6, 14, (il.fn_frames + il.fnf_ptr_word) as u32);
        a.sub_imm(16, 16, 1);
        a.movz(5, 24, 0);
        a.madd(6, 16, 5, 6);
        a.ldur(9, 6, 16); // FnFrame.extra
        a.cbnz(9, true, slow);
        // frame-pool room: len < 64 (the pool's policy cap) and len < capacity (a push must
        // not reallocate the Vec from machine code). Value drops below never touch the pool,
        // the frame stack, or the depth, so validating here stays sound.
        a.ldr_imm(7, 14, (il.frame_pool + il.fp_len_word) as u32);
        a.cmp_imm_x(7, 64);
        a.b_cond(C_HS, slow);
        a.ldr_imm(4, 14, (il.frame_pool + il.fp_cap_word) as u32);
        a.cmp_reg_x(7, 4);
        a.b_cond(C_HS, slow);
        // ---- commit ----
        // Drop the owned Values (callee `this`, then every slot). The strong count is re-read
        // PER VALUE, after all earlier decrements: two slots aliasing one object (`a = b = new
        // X` seeds several slots from one allocation) must route the LAST reference to a real
        // drop — a snapshot-validated bare dec would zero the count without ever running the
        // destructor and leak the whole subgraph (Splay's splay_ dummy node caught exactly
        // that). Bare dec when shared; H_DROP_AT (full drop, may cascade) for a last reference
        // or a BigInt. Only x9 (cursor) and x5 (remaining) survive the helper: spilled around
        // the call, everything else re-read afterwards.
        let drop_at = |a: &mut asm::Asm, value_reg: u32| {
            // x<value_reg> = address of the Value to drop; clobbers x0-x17 minus the spills.
            a.stp_pre(9, 5, -16);
            a.mov(2, value_reg);
            a.mov(0, 19);
            a.movz(1, 0, 0);
            a.ldr_imm(16, 21, (H_DROP_AT * 8) as u32);
            a.blr(16);
            a.ldp_post(9, 5, 16);
        };
        // callee `this` (the caller's restore overwrites the 16 bytes right after the stub)
        let this_done = a.new_label();
        let this_drop = a.new_label();
        a.ldrb_imm(9, 19, cx_this);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_LO, this_done);
        a.b_cond(C_EQ, this_drop); // BigInt → full drop
        a.ldr_imm(10, 19, cx_this + 8);
        a.ldur(11, 10, 0); // strong (rc contract: payload+0)
        a.cmp_imm_x(11, 1);
        a.b_cond(C_LS, this_drop); // last reference → full drop
        a.sub_imm(11, 11, 1);
        a.stur(11, 10, 0);
        a.b(this_done);
        a.bind(this_drop);
        a.add_imm(9, 19, cx_this);
        drop_at(a, 9);
        a.bind(this_done);
        // slots
        let c_loop = a.new_label();
        let c_next = a.new_label();
        let c_drop = a.new_label();
        let c_done = a.new_label();
        a.ldr_imm(9, 19, cx_slots);
        a.ldr_imm(5, 19, cx_n_slots);
        a.bind(c_loop);
        a.cbz(5, true, c_done);
        if PACKED_LOCAL_SLOTS {
            a.ldur(11, 9, 0);
            a.lsr_imm(13, 11, 48);
            a.movz(12, (crate::value::PACK_BIGINT >> 48) as u32, 0);
            a.cmp_reg_x(13, 12);
            a.b_cond(C_EQ, c_drop);
            let packed_ref = a.new_label();
            for tag in [
                crate::value::PACK_STR,
                crate::value::PACK_SYM,
                crate::value::PACK_OBJ,
            ] {
                a.movz(12, (tag >> 48) as u32, 0);
                a.cmp_reg_x(13, 12);
                a.b_cond(C_EQ, packed_ref);
            }
            a.b(c_next);
            a.bind(packed_ref);
            a.lsl_imm(12, 11, 16);
            a.lsr_imm(12, 12, 16);
        } else {
            a.ldrb_imm(11, 9, 0);
            a.cmp_imm_w(11, 5);
            a.b_cond(C_LO, c_next);
            a.b_cond(C_EQ, c_drop); // BigInt
            a.ldr_imm(12, 9, 8);
        }
        a.ldur(13, 12, 0);
        a.cmp_imm_x(13, 1);
        a.b_cond(C_LS, c_drop); // last reference
        a.sub_imm(13, 13, 1);
        a.stur(13, 12, 0);
        a.b(c_next);
        a.bind(c_drop);
        drop_at(a, 9);
        a.bind(c_next);
        a.add_imm(9, 9, if PACKED_LOCAL_SLOTS { 8 } else { 16 });
        a.sub_imm(5, 5, 1);
        a.b(c_loop);
        a.bind(c_done);
        // Bookkeeping (x14/x16/x7 may be stale after helper drops: re-read everything).
        a.ldr_imm(14, 19, 72); // ctx.interp
        // FnFrame pop
        a.ldr_imm(16, 14, (il.fn_frames + il.fnf_len_word) as u32);
        a.sub_imm(16, 16, 1);
        a.str_imm(16, 14, (il.fn_frames + il.fnf_len_word) as u32);
        // frame-pool push: ptr[len] = ctx.slots; len++ (room validated above; drops can't
        // have grown the pool)
        a.ldr_imm(7, 14, (il.frame_pool + il.fp_len_word) as u32);
        a.ldr_imm(4, 14, (il.frame_pool + il.fp_ptr_word) as u32);
        a.ldr_imm(6, 19, cx_slots);
        a.add_shifted(4, 4, 7, 3);
        a.stur(6, 4, 0);
        a.add_imm(7, 7, 1);
        a.str_imm(7, 14, (il.frame_pool + il.fp_len_word) as u32);
        // depth--
        a.ldr_w_imm(4, 14, il.depth as u32);
        a.sub_imm(4, 4, 1);
        a.str_w_imm(4, 14, il.depth as u32);
        a.movz(8, 0, 0); // not threw
        a.b(done);
    }
    // ---- helper fallback (nothing mutated above: `slow` is only reachable pre-commit) ----
    a.bind(slow);
    a.mov(0, 19);
    a.ldr_imm(16, 21, (H_DIRECT_FINISH * 8) as u32);
    a.blr(16);
    a.mov(8, 0);
    a.bind(done);
    a.ldp_post(29, 30, 16);
    a.ret();
}

/// Same immediate-range gate as [`get_prop_inlinable`] plus the `proto` offset (GetMethod walks
/// one prototype hop).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn get_method_inlinable(layout: &crate::value::JitLayout) -> bool {
    get_prop_inlinable(layout) && layout.obj_proto < 4096
}

/// Same gate as [`get_prop_inlinable`] plus the `writable` byte (the store re-checks it — an
/// in-place defineProperty can flip attributes without changing the shape).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn set_prop_inlinable(layout: &crate::value::JitLayout) -> bool {
    let cap = layout.obj_props + layout.props_entries + layout.vec_cap_off;
    get_prop_inlinable(layout)
        && layout.entry_writable < 256
        && layout.entry_accessor < 256
        && layout.obj_extensible < 4096
        && layout.obj_proto.is_multiple_of(8)
        && layout.obj_proto / 8 < 4096
        && layout.obj_props + layout.props_proto_flag < 4096
        && layout.props_elems.is_multiple_of(8)
        && layout.props_elems / 8 < 4096
        && cap.is_multiple_of(8)
        && cap / 8 < 4096
        && layout.gc_data_off < 4096
        && layout.entry_key + layout.str_ptr_word < 256
        && layout.entry_key + layout.str_len_word < 256
}

/// Inline `this.x++` / `--` (`UpdateProp`): the read and the write both target the cached own
/// data slot — exactly what a depth-0 IC hit on the VM path does (`get_prop_ic` then
/// `set_prop_ic`) — so a shape-validated receiver whose slot holds a Num updates in place with
/// one FP add. Anything else (accessor, non-writable, non-Num old value, shape/depth miss,
/// exotic receiver, last-reference receiver) falls to the checked helper before any state is
/// written.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    // 1. stack: [obj @ -16] — receiver must be an Obj with refcount > 1
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldur(10, 20, -8);
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
    a.ldr_imm(
        16,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
    );
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    guard_prop_data(a, 9, 15, ea, slow);
    guard_prop_writable(a, 9, 15, ew, slow);
    if layout.entry_accessor == layout.entry_value + 8 {
        a.ldur(16, 15, ev);
        a.lsr_imm(9, 16, 48);
        let number = a.new_label();
        a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_EQ, slow);
        a.movz(14, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_LO, number);
        a.movz(14, (crate::value::PACK_SYM >> 48) as u32, 0);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_LS, slow);
        a.bind(number);
    } else {
        a.ldurb(9, 15, ev);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, slow);
    }
    // --- commit: d0 = old, d2 = old ± 1, written in place ---
    a.ldur_d(
        0,
        15,
        if layout.entry_accessor == layout.entry_value + 8 {
            ev
        } else {
            ev + 8
        },
    );
    a.fmov_one(1);
    let dec = matches!(
        kind,
        UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
    );
    a.f_arith(if dec { 1 } else { 0 }, 2, 0, 1);
    a.stur_d(
        2,
        15,
        if layout.entry_accessor == layout.entry_value + 8 {
            ev
        } else {
            ev + 8
        },
    );
    // drop the receiver (strong was > 1)
    a.ldur(9, 10, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, strong);
    // result per kind: Pre* push the new value, Post* the old, *Discard nothing.
    match kind {
        UpdKind::PreInc | UpdKind::PreDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, -16);
            a.stur_d(2, 20, -8);
        }
        UpdKind::PostInc | UpdKind::PostDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, -16);
            a.stur_d(0, 20, -8);
        }
        UpdKind::IncDiscard | UpdKind::DecDiscard => {
            a.sub_imm(20, 20, 16);
        }
    }
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Gate for the inline equality / Not templates: the Obj arms read the receiver's `ic_plain`
/// byte, so those offsets must fit their instructions' immediate ranges.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn eq_inlinable(layout: &crate::value::JitLayout) -> bool {
    layout.valid
        && layout.rc_strong_off < 256
        && layout.obj_from_rc < 4096
        && layout.obj_ic_plain < 4096
        && crate::lstr::LEN_OFF.is_multiple_of(4)
        && crate::lstr::LEN_OFF / 4 < 4096
}

/// Fused `LoadLocal(a); LoadLocal(b); equality; JumpIfFalse`: compare borrowed frame values
/// directly for object identity and nullish cases. These cases require neither coercion nor
/// ownership changes. Any TDZ value, coercing mixed pair, or HTMLDDA/nullish pair replays the
/// original operations through their checked helpers before the frame is touched.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_local_eq_branch(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    lhs_off: u32,
    rhs_off: u32,
    first_pc: u32,
    l_unwind: usize,
    strict: bool,
    negate: bool,
    target: usize,
) {
    let slow = a.new_label();
    let done = a.new_label();
    let lhs_obj = a.new_label();
    let rhs_obj = a.new_label();
    let both_obj = a.new_label();
    let lhs_nullish = a.new_label();
    let rhs_nullish = a.new_label();
    let equal = a.new_label();
    let unequal = a.new_label();

    // w9/w10 are the borrowed Value tags. Empty is a TDZ sentinel, so it must retain the
    // checked LoadLocal path and its precise ReferenceError.
    a.ldrb_imm(9, 22, lhs_off);
    a.ldrb_imm(10, 22, rhs_off);
    a.cmp_imm_w(9, 1);
    a.b_cond(C_EQ, slow);
    a.cmp_imm_w(10, 1);
    a.b_cond(C_EQ, slow);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_EQ, lhs_obj);
    a.cmp_imm_w(10, 8);
    a.b_cond(C_EQ, rhs_obj);

    // Neither side is an object. Null/undefined compare loosely equal only to each other;
    // strictly they must have the same tag. Other strict different-tag pairs are definitively
    // unequal, while same-tag and loose primitive pairs retain their value/coercion helpers.
    a.cmp_imm_w(9, 0);
    a.b_cond(C_EQ, lhs_nullish);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_EQ, lhs_nullish);
    a.cmp_imm_w(10, 0);
    a.b_cond(C_EQ, rhs_nullish);
    a.cmp_imm_w(10, 2);
    a.b_cond(C_EQ, rhs_nullish);
    if strict {
        a.cmp_reg_w(9, 10);
        a.b_cond(C_NE, unequal);
    }
    a.b(slow);

    a.bind(lhs_nullish);
    if strict {
        a.cmp_reg_w(9, 10);
        a.b_cond(C_EQ, equal);
        a.b(unequal);
    } else {
        a.cmp_imm_w(10, 0);
        a.b_cond(C_EQ, equal);
        a.cmp_imm_w(10, 2);
        a.b_cond(C_EQ, equal);
        a.b(unequal);
    }

    a.bind(rhs_nullish);
    a.b(unequal); // lhs was already proven non-nullish

    // Object/object equality is identity for both strict and loose operators.
    a.bind(lhs_obj);
    a.cmp_imm_w(10, 8);
    a.b_cond(C_EQ, both_obj);
    if strict {
        a.b(unequal);
    } else {
        a.cmp_imm_w(10, 0);
        let lhs_null_cmp = a.new_label();
        a.b_cond(C_EQ, lhs_null_cmp);
        a.cmp_imm_w(10, 2);
        a.b_cond(C_NE, slow);
        a.bind(lhs_null_cmp);
        // The sole object/nullish exception is [[IsHTMLDDA]]. `ic_plain` false sends it back to
        // the helper; an ordinary object is definitively unequal to nullish.
        a.ldr_imm(12, 22, lhs_off + 8);
        a.add_imm(12, 12, layout.obj_from_rc as u32);
        a.ldrb_imm(12, 12, layout.obj_ic_plain as u32);
        a.cbz(12, false, slow);
        a.b(unequal);
    }

    a.bind(rhs_obj);
    if strict {
        a.b(unequal);
    } else {
        a.cmp_imm_w(9, 0);
        let rhs_null_cmp = a.new_label();
        a.b_cond(C_EQ, rhs_null_cmp);
        a.cmp_imm_w(9, 2);
        a.b_cond(C_NE, slow);
        a.bind(rhs_null_cmp);
        a.ldr_imm(12, 22, rhs_off + 8);
        a.add_imm(12, 12, layout.obj_from_rc as u32);
        a.ldrb_imm(12, 12, layout.obj_ic_plain as u32);
        a.cbz(12, false, slow);
        a.b(unequal);
    }

    a.bind(both_obj);
    a.ldr_imm(12, 22, lhs_off + 8);
    a.ldr_imm(13, 22, rhs_off + 8);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, equal);
    a.b(unequal);

    // JumpIfFalse branches when `(equal XOR negate)` is false.
    a.bind(equal);
    if negate {
        a.b(target);
    } else {
        a.b(done);
    }
    a.bind(unequal);
    if negate {
        a.b(done);
    } else {
        a.b(target);
    }

    a.bind(slow);
    emit_exec(a, first_pc, l_unwind);
    emit_exec(a, first_pc + 1, l_unwind);
    emit_exec(a, first_pc + 2, l_unwind);
    emit_cond(a, COND_POP_TRUTHY, l_unwind);
    a.cbz(1, false, target);
    a.bind(done);
}

/// Gate for the ordinary-constructor `instanceof` template. The current heap property layout is
/// NaN-boxed; require that exact form so decoding `.prototype` remains fail-closed if storage is
/// changed again.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn instanceof_inlinable(
    layout: &crate::value::JitLayout,
    il: &crate::interpreter::InterpLayout,
) -> bool {
    let sh = layout.obj_props + layout.props_shape;
    let en = layout.obj_props + layout.props_entries + layout.vec_ptr_off;
    let enl = layout.obj_props + layout.props_entries + layout.vec_len_off;
    layout.valid
        && il.valid
        && layout.entry_accessor == layout.entry_value + 8
        && layout.obj_from_rc < 4096
        && layout.obj_proto.is_multiple_of(8)
        && layout.obj_proto / 8 < 4096
        && layout.obj_exotic < 4096
        && layout.obj_ic_plain < 4096
        && layout.obj_is_constructor < 4096
        && sh.is_multiple_of(4)
        && sh / 4 < 4096
        && en.is_multiple_of(8)
        && en / 8 < 4096
        && enl.is_multiple_of(8)
        && enl / 8 < 4096
        && layout.entry_accessor < 4096
        && layout.entry_value < 256
        && layout.rc_strong_off < 256
        && layout.entry_size < 0x1_0000
        && il.function_proto.is_multiple_of(8)
        && il.function_proto / 8 < 4096
}

/// Inline the default OrdinaryHasInstance case. A single cache cell proves that the RHS still
/// has the key set observed by the checked path (notably, no own `@@hasInstance`); live guards
/// additionally validate constructor identity facts and decode its current `.prototype` value.
/// The LHS prototype walk is raw only while the realm-wide proxy latch remains clear. Every miss
/// occurs before stack/refcount mutation and therefore cleanly replays through `jit_exec`.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_instanceof_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    il: &crate::interpreter::InterpLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
) {
    use crate::bytecode::{IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT};
    let slow = a.new_label();
    let done = a.new_label();
    let ptr_same = a.new_label();
    let refs_ok = a.new_label();
    let walk = a.new_label();
    let yes = a.new_label();
    let no = a.new_label();
    let have = a.new_label();
    let strong = layout.rc_strong_off as i32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let enl = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;

    // Both operands must be objects, and no proxy-like object may participate anywhere in the
    // chain. x10/x11 retain the two stored Rc pointers until the final balanced decrements.
    a.ldurb(9, 20, -32);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldr_imm(9, 19, 32); // ctx.inline_ic_safe
    a.ldrb_imm(9, 9, 0);
    a.cbz(9, false, slow);
    a.ldur(10, 20, -24);
    a.ldur(11, 20, -8);

    // Popping the two Values must not run a destructor. Alias-aware validation mirrors the
    // equality template: the same Rc needs three live strong refs before subtracting two.
    a.cmp_reg_x(10, 11);
    a.b_cond(C_EQ, ptr_same);
    a.ldur(14, 10, strong);
    a.cmp_imm_x(14, 1);
    a.b_cond(C_LS, slow);
    a.ldur(15, 11, strong);
    a.cmp_imm_x(15, 1);
    a.b_cond(C_LS, slow);
    a.b(refs_ok);
    a.bind(ptr_same);
    a.ldur(14, 10, strong);
    a.cmp_imm_x(14, 2);
    a.b_cond(C_LS, slow);
    a.bind(refs_ok);

    // RHS: ordinary/plain constructor, canonical Function.prototype, and cached shape.
    a.add_imm(12, 11, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 12, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(9, 12, layout.obj_ic_plain as u32);
    a.cbz(9, false, slow);
    a.ldrb_imm(9, 12, layout.obj_is_constructor as u32);
    a.cbz(9, false, slow);
    a.ldr_imm(14, 12, layout.obj_proto as u32);
    a.ldr_imm(15, 19, 72); // ctx.interp
    a.ldr_imm(15, 15, il.function_proto as u32);
    a.cmp_reg_x(14, 15);
    a.b_cond(C_NE, slow);
    a.mov_imm64(13, cache_ptr as u64);
    a.ldrb_imm(9, 13, IC_OFF_DEPTH);
    a.cmp_imm_w(9, 0);
    a.b_cond(C_NE, slow);
    a.ldr_w_imm(14, 12, sh);
    a.ldr_w_imm(15, 13, IC_OFF_RECV_SHAPE);
    a.cmp_reg_w(14, 15);
    a.b_cond(C_NE, slow);

    // Resolve the cached own `.prototype` slot defensively, require a data property holding an
    // object, and untag its stored Rc pointer from the packed heap value.
    a.ldr_w_imm(13, 13, IC_OFF_SLOT);
    a.ldr_imm(14, 12, enl);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_HS, slow);
    a.ldr_imm(15, 12, en);
    a.mov_imm64(16, layout.entry_size as u64);
    a.madd(15, 13, 16, 15);
    guard_prop_data(a, 9, 15, layout.entry_accessor as u32, slow);
    a.ldur(15, 15, layout.entry_value as i32);
    a.lsr_imm(16, 15, 48);
    a.movz(17, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(16, 17);
    a.b_cond(C_NE, slow);
    a.lsl_imm(15, 15, 16);
    a.lsr_imm(15, 15, 16); // x15 = target prototype stored Rc pointer

    // Walk lhs.[[Prototype]] until target or null. Cycles are rejected by SetPrototypeOf, and
    // the proxy latch above makes every hop a direct Object::proto read.
    a.mov(12, 10);
    a.bind(walk);
    a.add_imm(12, 12, layout.obj_from_rc as u32);
    a.ldr_imm(12, 12, layout.obj_proto as u32);
    a.cbz(12, true, no);
    a.cmp_reg_x(12, 15);
    a.b_cond(C_EQ, yes);
    a.b(walk);
    a.bind(yes);
    a.movz(9, 1, 0);
    a.b(have);
    a.bind(no);
    a.movz(9, 0, 0);
    a.bind(have);

    // Commit: all guards have passed. Drop both object handles without reaching zero, replace
    // the two inputs with one Bool, and leave the stack in the normal binary-op shape.
    a.ldur(14, 10, strong);
    a.sub_imm(14, 14, 1);
    a.stur(14, 10, strong);
    a.ldur(14, 11, strong);
    a.sub_imm(14, 14, 1);
    a.stur(14, 11, strong);
    a.sub_imm(20, 20, 32);
    a.movz(10, 3, 0);
    a.stur(10, 20, 0);
    a.sturb(9, 20, 1);
    a.add_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Inline own-property store (`this.x = v`, statement position → `SetPropDrop`): the machine-code
/// mirror of `Interp::try_ic_set`'s shape fast path. Validates the receiver by shape (a match
/// proves the cached slot still maps this name), re-checks `accessor`/`writable`, then *moves*
/// the 16-byte value off the operand stack into the slot — a pure value overwrite never changes
/// the shape, so no cache invalidation is needed. The old value drops inline (strong-- when
/// refcounted and not the last reference); a BigInt old value (compound drop), a last-reference
/// old value or receiver, an accessor/non-writable slot, a shape or depth miss, and any exotic
/// receiver all fall to the checked helper. Every guard branches to `slow` before any state is
/// written, so the fallback re-runs the op cleanly.
///
/// Probe one polymorphic property-creation way. Entry has x11 at the receiver Object and the
/// incoming value at sp-16. A hit leaves x12 at its IcState and x13 holding the current entries
/// length, then branches to `commit`; a miss has no side effects.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_prop_create_probe(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    miss: usize,
    commit: usize,
) {
    use crate::bytecode::{
        IC_CREATE, IC_OFF_DEPTH, IC_OFF_MID2_SHAPE, IC_OFF_MID_SHAPE, IC_OFF_RECV_SHAPE,
        IC_OFF_SLOT,
    };
    let sh = (layout.obj_props + layout.props_shape) as u32;
    a.mov_imm64(12, cache_ptr as u64);
    a.ldrb_imm(9, 12, IC_OFF_DEPTH);
    a.cmp_imm_w(9, IC_CREATE as u32);
    a.b_cond(C_NE, miss);
    a.ldr_w_imm(13, 11, sh);
    a.ldr_w_imm(14, 12, IC_OFF_RECV_SHAPE);
    a.cmp_reg_w(13, 14);
    a.b_cond(C_NE, miss);
    a.ldrb_imm(9, 11, layout.obj_extensible as u32);
    a.cbz(9, false, miss);
    a.ldrb_imm(
        9,
        11,
        (layout.obj_props + layout.props_proto_flag) as u32,
    );
    a.cbnz(9, false, miss);
    // Named-only small map: no DenseStorage sidecar/index, <=8 entries, and spare capacity.
    a.ldr_imm(9, 11, (layout.obj_props + layout.props_elems) as u32);
    a.cbnz(9, true, miss);
    let len_off = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    let cap_off = (layout.obj_props + layout.props_entries + layout.vec_cap_off) as u32;
    a.ldr_imm(13, 11, len_off);
    a.cmp_imm_x(13, 8);
    a.b_cond(C_HS, miss);
    a.ldr_imm(14, 11, cap_off);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_HS, miss);
    // Same live global epoch recorded by the fill; saturation is never cacheable.
    a.mov_imm64(16, crate::value::proto_epoch_ptr() as usize as u64);
    a.ldr_w_imm(16, 16, 0);
    a.ldr_w_imm(17, 12, IC_OFF_MID_SHAPE);
    a.cmp_reg_w(16, 17);
    a.b_cond(C_NE, miss);
    a.mov_imm64(9, u32::MAX as u64);
    a.cmp_reg_w(16, 9);
    a.b_cond(C_EQ, miss);
    // Cache stores Rc::as_ptr(proto); Object::proto stores the RcBox pointer.
    a.ldr_w_imm(14, 12, IC_OFF_SLOT);
    a.ldr_w_imm(15, 12, IC_OFF_MID2_SHAPE);
    a.lsl_imm(15, 15, 32);
    a.logic_x(1, 14, 14, 15);
    a.ldr_imm(16, 11, layout.obj_proto as u32);
    let proto_ready = a.new_label();
    a.cbz(16, true, proto_ready);
    a.add_imm(16, 16, layout.gc_data_off as u32);
    a.bind(proto_ready);
    a.cmp_reg_x(14, 16);
    a.b_cond(C_NE, miss);
    // BigInt packing owns compound storage; all other Values can transfer stack ownership.
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, miss);
    a.b(commit);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_set_prop_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    name: &str,
    pc: u32,
    l_unwind: usize,
    recv: PropRecv,
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
    // 1. receiver must be an Obj (tag 8). Stack form: [obj @ -32, v @ -16], refcount-managed;
    // this/slot forms: [v @ -16] only, the frame owns the receiver.
    match recv {
        PropRecv::Stack => {
            a.ldurb(9, 20, -32);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, slow);
            a.ldur(10, 20, -24); // receiver rc_ptr
            // receiver refcount > 1 (so the pop-drop below never frees)
            a.ldur(9, 10, strong);
            a.cmp_imm_x(9, 1);
            a.b_cond(C_LS, slow);
        }
        PropRecv::This => {
            a.ldr_imm(14, 19, 48); // ctx.this_raw
            a.ldurb(9, 14, 0);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, slow);
            a.ldur(10, 14, 8);
        }
        PropRecv::Slot(off) => {
            a.ldrb_imm(9, 22, off);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, slow);
            a.ldr_imm(10, 22, off + 8);
        }
    }
    // 2. object base; exotic None, and not a side-table exotic (proxy/typed-array/namespace).
    //    Receiver-wide facts — validated once, shared by both ways.
    a.add_imm(11, 10, rcv);
    a.ldrb_imm(9, 11, ex);
    a.cmp_imm_w(9, none_tag);
    a.b_cond(C_NE, slow);
    a.ldrb_imm(9, 11, plain);
    a.cbz(9, false, slow);
    // Creation IC: constructors repeatedly assign a named field to a fresh receiver whose map
    // lacks it. Reuse the checked path's shape/epoch/prototype proof and append directly into
    // already-reserved Vec capacity. Small named-only maps have no index/dense sidecar, so the
    // append has no secondary structure to maintain and performs no allocation.
    if layout.key_probe_ok
        && !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name != "length"
        && name != "prototype"
    {
        use crate::bytecode::IC_OFF_HOLDER_SHAPE;
        let not_create = a.new_label();
        let create_commit = a.new_label();
        let len_off = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
        let stride = std::mem::size_of::<std::cell::Cell<crate::bytecode::IcState>>();
        for way in 0..crate::bytecode::PROP_IC_WAYS {
            let miss = if way + 1 == crate::bytecode::PROP_IC_WAYS {
                not_create
            } else {
                a.new_label()
            };
            let way_ptr = cache_ptr + way * stride;
            emit_prop_create_probe(a, layout, way_ptr, miss, create_commit);
            if way + 1 != crate::bytecode::PROP_IC_WAYS {
                a.bind(miss);
            }
        }

        a.bind(create_commit);
        // From here no branch can fail: compute the vacant entry and pack the incoming value.
        a.ldr_imm(15, 11, en);
        a.mov_imm64(16, es);
        a.madd(15, 13, 16, 15);
        a.ldur(16, 20, -8);
        let packed = a.new_label();
        let is_undefined = a.new_label();
        let is_empty = a.new_label();
        let is_null = a.new_label();
        let is_bool = a.new_label();
        let is_str = a.new_label();
        let is_sym = a.new_label();
        let is_obj = a.new_label();
        for (tag, label) in [
            (0, is_undefined),
            (1, is_empty),
            (2, is_null),
            (3, is_bool),
            (6, is_str),
            (7, is_sym),
            (8, is_obj),
        ] {
            a.cmp_imm_w(9, tag);
            a.b_cond(C_EQ, label);
        }
        a.b(packed); // Number payload is already its packed representation.
        for (label, bits) in [
            (is_undefined, crate::value::PACK_UNDEFINED),
            (is_empty, crate::value::PACK_EMPTY),
            (is_null, crate::value::PACK_NULL),
        ] {
            a.bind(label);
            a.mov_imm64(16, bits);
            a.b(packed);
        }
        a.bind(is_bool);
        a.ldurb(16, 20, -15);
        a.mov_imm64(14, crate::value::PACK_BOOL);
        a.logic_x(1, 16, 16, 14);
        a.b(packed);
        for (label, bits) in [
            (is_str, crate::value::PACK_STR),
            (is_sym, crate::value::PACK_SYM),
            (is_obj, crate::value::PACK_OBJ),
        ] {
            a.bind(label);
            a.mov_imm64(14, bits);
            a.logic_x(1, 16, 16, 14);
            a.b(packed);
        }
        a.bind(packed);
        // Clone the site's pinned Rc<str> into the tuple entry, then install Property::plain.
        let key_stored = name.as_ptr() as usize - layout.str_data_off;
        a.mov_imm64(17, key_stored as u64);
        a.ldur(14, 17, strong);
        a.add_imm(14, 14, 1);
        a.stur(14, 17, strong);
        a.stur(
            17,
            15,
            (layout.entry_key + layout.str_ptr_word) as i32,
        );
        a.mov_imm64(14, name.len() as u64);
        a.stur(
            14,
            15,
            (layout.entry_key + layout.str_len_word) as i32,
        );
        a.stur(16, 15, ev);
        a.movz(
            14,
            (crate::value::PROP_WRITABLE
                | crate::value::PROP_ENUMERABLE
                | crate::value::PROP_CONFIGURABLE) as u32,
            0,
        );
        a.stur(14, 15, ea as i32);
        // Publish the entry by updating shape then length. No allocation or side structure is
        // touched; the stack Value's refcounted payload ownership moved into the packed slot.
        a.ldr_w_imm(14, 12, IC_OFF_HOLDER_SHAPE);
        a.str_w_imm(14, 11, sh);
        a.add_imm(13, 13, 1);
        a.str_imm(13, 11, len_off);
        if matches!(recv, PropRecv::Stack) {
            a.ldur(9, 10, strong);
            a.sub_imm(9, 9, 1);
            a.stur(9, 10, strong);
            a.sub_imm(20, 20, 32);
        } else {
            a.sub_imm(20, 20, 16);
        }
        a.b(done);
        a.bind(not_create);
    }
    // 3-8 per way (sites allocate PROP_IC_WAYS consecutive cells; the Rust fast path probes
    // all of them, so the template must too or a rotating store site helper-calls forever):
    // depth 0, shape match, slot bounds, data+writable, old-value droppability. Guard misses
    // jump to the next way, the last way's to the helper. Register results consumed by the
    // commit below: x13 slot, x15 entry, w9 old tag, x12 old payload, x14 old strong.
    // `cache_ptr` = the IcState cell address, or 0 for "x12 already holds it" (the way loop
    // keeps its cursor in x8: the body clobbers x12 on the old-value path).
    let commit = a.new_label();
    let way = |a: &mut asm::Asm, cache_ptr: usize, miss: usize| {
        if cache_ptr != 0 {
            a.mov_imm64(12, cache_ptr as u64);
        }
        a.ldrb_imm(9, 12, IC_OFF_DEPTH);
        a.cbnz(9, false, miss);
        a.ldr_w_imm(13, 12, IC_OFF_SLOT);
        a.ldr_w_imm(14, 12, IC_OFF_RECV_SHAPE);
        a.ldr_w_imm(9, 11, sh);
        a.cmp_reg_w(9, 14);
        a.b_cond(C_NE, miss);
        a.ldr_imm(
            16,
            11,
            (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
        );
        a.cmp_reg_x(13, 16);
        a.b_cond(C_HS, miss);
        a.ldr_imm(15, 11, en);
        a.mov_imm64(16, es);
        a.madd(15, 13, 16, 15);
        guard_prop_data(a, 9, 15, ea, miss);
        guard_prop_writable(a, 9, 15, ew, miss);
        // old value: trivially droppable (tag ≤ 4), or refcounted with strong > 1 (inline
        // dec); BigInt or a last reference → helper. An old value that IS the receiver
        // (`o.x === o`) also bails: its dec and the receiver dec below hit the same counter,
        // and the two independent strong > 1 guards would let the pair scribble it to 0
        // without running the destructor.
        if layout.entry_accessor == layout.entry_value + 8 {
            a.ldur(12, 15, ev);
            a.lsr_imm(9, 12, 48);
            a.movz(14, (crate::value::PACK_BIGINT >> 48) as u32, 0);
            a.cmp_reg_x(9, 14);
            a.b_cond(C_EQ, miss);
            let old_ref = a.new_label();
            let old_plain = a.new_label();
            for tag in [
                crate::value::PACK_STR,
                crate::value::PACK_SYM,
                crate::value::PACK_OBJ,
            ] {
                a.movz(14, (tag >> 48) as u32, 0);
                a.cmp_reg_x(9, 14);
                a.b_cond(C_EQ, old_ref);
            }
            a.movz(9, 0, 0); // commit marker: no old refcount decrement
            a.b(old_plain);
            a.bind(old_ref);
            a.lsl_imm(12, 12, 16);
            a.lsr_imm(12, 12, 16);
            a.cmp_reg_x(12, 10);
            a.b_cond(C_EQ, miss);
            a.ldur(14, 12, strong);
            a.cmp_imm_x(14, 1);
            a.b_cond(C_LS, miss);
            a.movz(9, 6, 0); // any refcounted old value
            a.bind(old_plain);
        } else {
            a.ldurb(9, 15, ev);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_EQ, miss);
            let old_plain = a.new_label();
            a.cmp_imm_w(9, 6);
            a.b_cond(C_LO, old_plain);
            a.ldur(12, 15, ev + 8);
            a.cmp_reg_x(12, 10);
            a.b_cond(C_EQ, miss);
            a.ldur(14, 12, strong);
            a.cmp_imm_x(14, 1);
            a.b_cond(C_LS, miss);
            a.bind(old_plain);
        }
    };
    {
        let l_way = a.new_label();
        let l_way_next = a.new_label();
        a.mov_imm64(8, cache_ptr as u64);
        a.movz(7, crate::bytecode::PROP_IC_WAYS as u32, 0);
        a.bind(l_way);
        a.mov(12, 8);
        way(a, 0, l_way_next);
        a.b(commit);
        a.bind(l_way_next);
        let ic_stride = std::mem::size_of::<std::cell::Cell<crate::bytecode::IcState>>();
        a.add_imm(8, 8, ic_stride as u32);
        a.sub_imm(7, 7, 1);
        a.cbnz(7, false, l_way);
        a.b(slow);
    }
    a.bind(commit);
    // --- commit: everything validated; from here only writes ---
    // Move v into the entry. Packed storage encodes the wide stack value in x16; ownership of a
    // refcounted payload transfers unchanged from the stack slot into the property.
    a.ldurb(13, 20, -16);
    a.ldur(16, 20, -8);
    if layout.entry_accessor == layout.entry_value + 8 {
        a.cmp_imm_w(13, 5);
        a.b_cond(C_EQ, slow); // BigInt's compound path stays checked
        let packed = a.new_label();
        let is_undefined = a.new_label();
        let is_empty = a.new_label();
        let is_null = a.new_label();
        let is_bool = a.new_label();
        let is_str = a.new_label();
        let is_sym = a.new_label();
        let is_obj = a.new_label();
        for (tag, label) in [
            (0, is_undefined),
            (1, is_empty),
            (2, is_null),
            (3, is_bool),
            (6, is_str),
            (7, is_sym),
            (8, is_obj),
        ] {
            a.cmp_imm_w(13, tag);
            a.b_cond(C_EQ, label);
        }
        // Number: payload bits are already the packed representation.
        a.b(packed);
        for (label, bits) in [
            (is_undefined, crate::value::PACK_UNDEFINED),
            (is_empty, crate::value::PACK_EMPTY),
            (is_null, crate::value::PACK_NULL),
        ] {
            a.bind(label);
            a.mov_imm64(16, bits);
            a.b(packed);
        }
        a.bind(is_bool);
        a.ldurb(16, 20, -15);
        a.mov_imm64(14, crate::value::PACK_BOOL);
        a.logic_x(1, 16, 16, 14);
        a.b(packed);
        for (label, bits) in [
            (is_str, crate::value::PACK_STR),
            (is_sym, crate::value::PACK_SYM),
            (is_obj, crate::value::PACK_OBJ),
        ] {
            a.bind(label);
            a.mov_imm64(14, bits);
            a.logic_x(1, 16, 16, 14);
            a.b(packed);
        }
        a.bind(packed);
        a.stur(16, 15, ev);
    } else {
        a.ldur(13, 20, -16);
        a.stur(13, 15, ev);
        a.stur(16, 15, ev + 8);
    }
    // drop the old value (refcounted: strong was > 1, so this never frees)
    let no_old_dec = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, no_old_dec);
    a.ldur(14, 12, strong);
    a.sub_imm(14, 14, 1);
    a.stur(14, 12, strong);
    a.bind(no_old_dec);
    if matches!(recv, PropRecv::Stack) {
        // drop the receiver (strong was > 1)
        a.ldur(9, 10, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 10, strong);
        // pop both operands, push nothing
        a.sub_imm(20, 20, 32);
    } else {
        // pop just the value
        a.sub_imm(20, 20, 16);
    }
    a.b(done);
    a.bind(slow);
    emit_op_helper(a, H_SET_PROP, pc, l_unwind);
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    // stack: [a @ -32, b @ -16]; w9 = tag_a, w10 = tag_b
    a.ldurb(9, 20, -32);
    a.ldurb(10, 20, -16);
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
        a.ldur(13, 20, -8);
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
        a.ldur(12, 20, -24);
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
    a.ldurb(12, 20, -31);
    a.ldurb(13, 20, -15);
    a.cmp_reg_w(12, 13);
    a.cset_w(11, C_EQ);
    a.b(l_have);
    // Sym/Obj identity: same pointer → equal (dec by 2; both stack handles die), different →
    // unequal (dec each; both guarded > 1 first so neither dec frees).
    a.bind(l_ptr);
    a.ldur(12, 20, -24);
    a.ldur(13, 20, -8);
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
    a.ldur(12, 20, -24);
    a.ldur(13, 20, -8);
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
        a.ldur(12, 20, -24);
        a.ldur(14, 12, strong);
        a.cmp_imm_x(14, 1);
        a.b_cond(C_LS, slow);
        a.bind(ga);
        let gb = a.new_label();
        a.cmp_imm_w(10, 6);
        a.b_cond(C_LO, gb);
        a.ldur(13, 20, -8);
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
    a.ldur_d(0, 20, -24);
    a.ldur_d(1, 20, -8);
    if let Some(target) = branch {
        // Straight-line fused numeric compare — branch on the negated condition, matching the
        // ordered-relation fusion (IEEE unordered must jump for == and fall through for !=).
        a.sub_imm(20, 20, 32);
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
    a.sub_imm(20, 20, 32);
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
            a.add_imm(20, 20, 16);
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    a.ldurb(9, 20, -16);
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
    a.ldurb(11, 20, -15);
    a.movz(12, 1, 0);
    a.logic_w(2, 11, 11, 12); // eor: flip
    a.b(l_have);
    a.bind(l_num);
    a.ldur_d(0, 20, -8);
    a.movz(12, 0, 0);
    a.fmov_d_x(1, 12); // d1 = +0.0
    a.fcmp(0, 1);
    a.cset_w(11, C_EQ); // ±0 → falsy
    a.cset_w(12, C_VS); // NaN (unordered) → falsy
    a.logic_w(1, 11, 11, 12); // orr
    a.b(l_have);
    a.bind(l_str);
    a.ldur(12, 20, -8);
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
    a.ldur(12, 20, -8);
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
    a.stur(10, 20, -16);
    a.sturb(11, 20, -15);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Gate for the inline LoadName template: probed layouts hold and every baked offset fits its
/// instruction's immediate range.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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

/// The cached numeric name update additionally writes through the resolved binding/property.
/// Require the descriptor and binding-mutability bytes to be directly addressable, and the
/// packed global-property representation understood by the emitted Number guard.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn update_name_inlinable(layout: &crate::value::JitLayout) -> bool {
    load_name_inlinable(layout)
        && layout.entry_accessor == layout.entry_value + 8
        && layout.entry_writable >= layout.entry_value
        && layout.entry_writable - layout.entry_value < 4096
        && layout.binding_mutable >= layout.binding_value
        && layout.binding_mutable - layout.binding_value < 4096
}

/// Inline `++`/`--` on a cached free name holding a Number. Cache validation proves the live
/// resolution and leaves x14 at the binding/property value. Mutable/writable and Number guards
/// all run before the FP update is committed; any mismatch replays the original op in Rust.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_update_name_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    kind: UpdKind,
    pc: u32,
    l_unwind: usize,
) {
    let slow = a.new_label();
    let done = a.new_label();
    let bits = a.new_label();
    let scope = a.new_label();

    // x14 -> wide Binding.value (x7=0) or packed global Property value (x7=1).
    emit_name_ic_value_ptr(a, layout, cache_ptr, slow, true);
    a.cbz(7, false, scope);

    // Global-object mode: the live data property must remain writable and contain a Number.
    guard_prop_writable(
        a,
        9,
        14,
        (layout.entry_writable - layout.entry_value) as u32,
        slow,
    );
    a.ldur(16, 14, 0);
    a.lsr_imm(9, 16, 48);
    let packed_number = a.new_label();
    a.movz(13, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_EQ, slow);
    a.movz(13, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_LO, packed_number);
    a.movz(13, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_HI, packed_number);
    a.b(slow);
    a.bind(packed_number);
    a.b(bits);

    // Scope mode: mutability is live (const/named-expression bindings must take the slow path);
    // wide execution Values carry their Number tag beside the payload.
    a.bind(scope);
    a.ldrb_imm(
        9,
        14,
        (layout.binding_mutable - layout.binding_value) as u32,
    );
    a.cbz(9, false, slow);
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    a.add_imm(14, 14, 8);

    // x14 now points at the f64 bits in either storage mode.
    a.bind(bits);
    a.ldur_d(0, 14, 0);
    a.fcmp(0, 0);
    a.b_cond(C_VS, slow); // keep NaN boxing/canonicalization on the checked path
    a.fmov_one(1);
    let dec = matches!(
        kind,
        UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
    );
    a.f_arith(if dec { 1 } else { 0 }, 2, 0, 1);
    a.stur_d(2, 14, 0);
    match kind {
        UpdKind::PreInc | UpdKind::PreDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, 0);
            a.stur_d(2, 20, 8);
            a.add_imm(20, 20, 16);
        }
        UpdKind::PostInc | UpdKind::PostDec => {
            a.movz(9, 4, 0);
            a.stur(9, 20, 0);
            a.stur_d(0, 20, 8);
            a.add_imm(20, 20, 16);
        }
        UpdKind::IncDiscard | UpdKind::DecDiscard => {}
    }
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Inline a cached free-name store when both the old and new values are non-owning scalar Values.
/// Refcounted payload replacement, immutable bindings, non-writable/accessor globals, NaN packing,
/// and every cache miss replay through the checked executor before any stack or target mutation.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_store_name_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    pc: u32,
    l_unwind: usize,
) {
    let slow = a.new_label();
    let done = a.new_label();
    let scope = a.new_label();
    let packed_commit = a.new_label();

    // Only scalar execution Values can move without refcount/destructor work.
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_HI, slow);
    emit_name_ic_value_ptr(a, layout, cache_ptr, slow, true);
    a.cbz(7, false, scope);

    // Packed global property: validate writability and prove the old owner is also scalar.
    guard_prop_writable(
        a,
        9,
        14,
        (layout.entry_writable - layout.entry_value) as u32,
        slow,
    );
    a.ldur(16, 14, 0);
    a.lsr_imm(9, 16, 48);
    let old_scalar = a.new_label();
    a.movz(13, (crate::value::PACK_BIGINT >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_LO, old_scalar);
    a.movz(13, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_LS, slow);
    a.movz(13, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_w(9, 13);
    a.b_cond(C_EQ, slow);
    a.bind(old_scalar);

    // Pack the new wide scalar into x10.
    a.ldurb(9, 20, -16);
    let pack_empty = a.new_label();
    let pack_null = a.new_label();
    let pack_bool = a.new_label();
    let pack_num = a.new_label();
    let pack_undefined = a.new_label();
    a.cbz(9, false, pack_undefined);
    a.cmp_imm_w(9, 1);
    a.b_cond(C_EQ, pack_empty);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_EQ, pack_null);
    a.cmp_imm_w(9, 3);
    a.b_cond(C_EQ, pack_bool);
    a.b(pack_num);
    a.bind(pack_undefined);
    a.mov_imm64(10, crate::value::PACK_UNDEFINED);
    a.b(packed_commit);
    a.bind(pack_empty);
    a.mov_imm64(10, crate::value::PACK_EMPTY);
    a.b(packed_commit);
    a.bind(pack_null);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.b(packed_commit);
    a.bind(pack_bool);
    a.mov_imm64(10, crate::value::PACK_BOOL);
    a.ldurb(11, 20, -15);
    a.logic_x(1, 10, 10, 11);
    a.b(packed_commit);
    a.bind(pack_num);
    a.ldur_d(0, 20, -8);
    a.fcmp(0, 0);
    a.b_cond(C_VS, slow);
    a.ldur(10, 20, -8);
    a.bind(packed_commit);
    a.stur(10, 14, 0);
    a.sub_imm(20, 20, 16);
    a.b(done);

    // Wide scope binding: validate mutability and prove replacing the old value needs no drop.
    a.bind(scope);
    a.ldrb_imm(
        9,
        14,
        (layout.binding_mutable - layout.binding_value) as u32,
    );
    a.cbz(9, false, slow);
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_HI, slow);
    a.ldur(9, 20, -16);
    a.ldur(10, 20, -8);
    a.stur(9, 14, 0);
    a.stur(10, 14, 8);
    a.sub_imm(20, 20, 16);
    a.b(done);

    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Inline free-name read (`LoadName`) against the per-site [`crate::bytecode::NameIc`]: compare
/// the live activation env pointer and the scope's binding-map generation, then copy the cached
/// binding's value straight out of the scope — no hashing, no helper call. The cache is filled
/// by the VM slow path (`Chunk::name_ic_fill`, depth-0 resolutions only); any mismatch — cold
/// cache, different env, structural scope change, TDZ, BigInt value — takes the checked helper.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_load_name_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    preferred_number: Option<u64>,
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
    emit_name_ic_value_ptr(a, layout, cache_ptr, slow, true);
    // Value not a BigInt → materialize the wide pair, bump if refcounted, push. x7 identifies
    // the packed global-property arm; scope bindings are already wide.
    let loaded = a.new_label();
    if layout.entry_accessor == layout.entry_value + 8 {
        let wide = a.new_label();
        a.cbz(7, false, wide);
        a.ldur(11, 14, 0);
        if let Some(bits) = preferred_number {
            // Hot script constants stay live and mutable: compare the packed global property on
            // every read, then widen the proven Number with no NaN-box tag decoder. Assignment
            // of a different value simply falls through to the generic live decoder below.
            let generic = a.new_label();
            a.mov_imm64(16, bits);
            a.cmp_reg_x(11, 16);
            a.b_cond(C_NE, generic);
            a.movz(10, 4, 0);
            a.b(loaded);
            a.bind(generic);
        }
        a.lsr_imm(9, 11, 48);
        let is_undefined = a.new_label();
        let is_empty = a.new_label();
        let is_null = a.new_label();
        let is_bool = a.new_label();
        let is_str = a.new_label();
        let is_sym = a.new_label();
        let is_obj = a.new_label();
        let is_number = a.new_label();
        a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_EQ, is_obj);
        a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_LO, is_number);
        a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_HI, is_number);
        for (tag, label) in [
            (crate::value::PACK_BOOL, is_bool),
            (crate::value::PACK_STR, is_str),
            (crate::value::PACK_SYM, is_sym),
            (crate::value::PACK_BIGINT, slow),
            (crate::value::PACK_UNDEFINED, is_undefined),
            (crate::value::PACK_EMPTY, is_empty),
            (crate::value::PACK_NULL, is_null),
        ] {
            a.movz(16, (tag >> 48) as u32, 0);
            a.cmp_reg_x(9, 16);
            a.b_cond(C_EQ, label);
        }
        a.bind(is_number);
        a.movz(10, 4, 0); // Number; x11 already holds its bits
        a.b(loaded);
        for (label, tag) in [(is_undefined, 0), (is_empty, 1), (is_null, 2)] {
            a.bind(label);
            a.movz(10, tag, 0);
            a.movz(11, 0, 0);
            a.b(loaded);
        }
        a.bind(is_bool);
        a.movz(10, 3, 0);
        a.lsl_imm_w(11, 11, 8);
        a.logic_w(1, 10, 10, 11);
        a.movz(11, 0, 0);
        a.b(loaded);
        for (label, tag) in [(is_str, 6), (is_sym, 7), (is_obj, 8)] {
            a.bind(label);
            a.movz(10, tag, 0);
            a.lsl_imm(11, 11, 16);
            a.lsr_imm(11, 11, 16);
            a.ldur(16, 11, strong);
            a.add_imm(16, 16, 1);
            a.stur(16, 11, strong);
            a.b(loaded);
        }
        a.bind(wide);
    }
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    a.ldur(10, 14, 0);
    a.ldur(11, 14, 8);
    let nobump = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, nobump);
    a.ldur(16, 11, strong);
    a.add_imm(16, 16, 1);
    a.stur(16, 11, strong);
    a.bind(nobump);
    a.bind(loaded);
    if for_call {
        a.stur(31, 20, 0);
        a.stur(31, 20, 8);
        a.add_imm(20, 20, 16);
    }
    a.stur(10, 20, 0);
    a.stur(11, 20, 8);
    a.add_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Shared LoadName cache validation: on success x14 points at the resolved `Value` (the binding's
/// value in scope mode, the global entry's value in global mode) and execution falls through; any
/// mismatch branches to `slow`. Clobbers x7 and x9-x17.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_name_ic_value_ptr(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache_ptr: usize,
    slow: usize,
    packed_ok: bool,
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
    if layout.entry_accessor == layout.entry_value + 8 && !packed_ok {
        // Global bindings live in packed properties; scope bindings below remain wide. Keep the
        // global arm checked until it shares the packed decoder with GetProp.
        a.b(slow);
    }
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
    guard_prop_data(a, 14, 15, g_ea, slow);
    a.add_imm(14, 15, g_ev); // x14 → the entry's Value
    a.movz(7, 1, 0); // packed global property
    let have = a.new_label();
    a.b(have);
    // --- scope mode: binding initialized (TDZ) ---
    a.bind(scope);
    a.ldr_imm(14, 12, NAME_IC_OFF_BINDING);
    a.ldrb_imm(9, 14, bi);
    a.cbz(9, false, slow);
    a.add_imm(14, 14, bv); // x14 → the binding's Value
    a.movz(7, 0, 0); // wide scope binding
    a.bind(have);
}

/// Same gate as [`get_prop_inlinable`] plus the dense-element (`Props::elems`) and
/// writable-flag offsets the element templates bake in.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn elem_inlinable(layout: &crate::value::JitLayout) -> bool {
    let elems = layout.obj_props + layout.props_elems;
    get_prop_inlinable(layout)
        && (layout.entry_accessor == layout.entry_value + 8
            || layout.entry_accessor >= layout.entry_value + 16)
        && [
            elems,
            layout.dense_elems + layout.vec_ptr_off,
            layout.dense_elems + layout.vec_len_off,
            layout.dense_mirror + layout.vec_ptr_off,
            layout.dense_mirror + layout.vec_len_off,
        ]
        .into_iter()
        .all(|off| off.is_multiple_of(8) && off / 8 < 4096)
        && layout.entry_writable < 4096
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn packed_elem_inlinable(layout: &crate::value::JitLayout) -> bool {
    layout.packed_elems_valid
        && layout.property_size == 16
        && layout.property_value < 256
        && layout.property_meta < 4096
        && layout.dense_packed.is_multiple_of(8)
        && layout.dense_packed / 8 < 4096
}

/// Packed entries can still use the numeric mirror read; the classic entry chase falls back.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn get_elem_inlinable(layout: &crate::value::JitLayout) -> bool {
    let elems = layout.obj_props + layout.props_elems;
    get_prop_inlinable(layout)
        && [
            elems,
            layout.dense_elems + layout.vec_ptr_off,
            layout.dense_elems + layout.vec_len_off,
            layout.dense_mirror + layout.vec_ptr_off,
            layout.dense_mirror + layout.vec_len_off,
        ]
        .into_iter()
        .all(|off| off.is_multiple_of(8) && off / 8 < 4096)
}

/// Inline dense-element read (`a[i]`): an own data element of a plain object/array, indexed
/// through `Props::elems` without hashing or stringifying the key — the machine-code mirror of
/// `Interp::fast_get_elem`. Every guard branches to `slow` before any state is written. Handles a
/// Num key that is exactly a u32 in dense bounds, a non-accessor slot, and a non-BigInt value on
/// a receiver that is not the last reference; the live `inline_ic_safe` flag rules out proxies /
/// typed arrays / module namespaces existing at all. Everything else falls to the checked helper.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_get_elem_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    pc: u32,
    l_unwind: usize,
) {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let el = (layout.obj_props + layout.props_elems) as u32;
    let evp = (layout.dense_elems + layout.vec_ptr_off) as u32;
    let evl = (layout.dense_elems + layout.vec_len_off) as u32;
    let mvp = (layout.dense_mirror + layout.vec_ptr_off) as u32;
    let mvl = (layout.dense_mirror + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;

    let plain = layout.obj_ic_plain as u32;
    let slow = a.new_label();
    let done = a.new_label();
    // 1. stack: [obj @ -32, key @ -16] — receiver must be Obj, key must be Num
    a.ldurb(9, 20, -32);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    // 2. key must be exactly a u32 (round-trip compare; NaN/negative/fractional/huge all miss)
    a.ldur_d(0, 20, -8);
    a.fcvtzu_w_d(9, 0);
    a.ucvtf_d_w(1, 9);
    a.fcmp(0, 1);
    a.b_cond(C_NE, slow);
    // 3. receiver refcount > 1 (so the pop-drop below never frees)
    a.ldur(10, 20, -24);
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
    // 5. mirror read: coherent + hole-free ⇒ bounds + one indexed load of a known Num — no
    // entry chase, no tag check, no refcount bump. A miss answers classically below.
    let classic = a.new_label();
    let mirror_hit = a.new_label();
    let mf = (layout.obj_props + layout.props_mirror_flags) as u32;
    let mirror = el;
    a.ldrb_imm(12, 11, mf);
    let mask = asm::logical_imm_w((crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32)
        .unwrap();
    a.logic_imm_w(0, 12, 12, mask);
    a.cmp_imm_w(
        12,
        (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
    );
    a.b_cond(C_NE, classic);
    a.ldr_imm(12, 11, mirror);
    a.cbz(12, true, classic);
    a.ldr_imm(14, 12, mvl);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, classic);
    a.ldr_imm(12, 12, mvp);
    a.ldr_d_lsl3(0, 12, 9);
    a.movz(12, 4, 0);
    a.fmov_x_d(13, 0);
    a.movz(14, 0, 0);
    a.b(mirror_hit); // a Num: skip the refcount-bump block
    a.bind(classic);
    // 5b. dense bounds: n < elems.len (x9's upper bits are zero from the w-form fcvtzu)
    a.ldr_imm(12, 11, el);
    a.cbz(12, true, slow);
    if packed_elem_inlinable(layout) {
        let classic_dense = a.new_label();
        a.ldr_imm(15, 12, layout.dense_packed as u32);
        a.cbz(15, true, classic_dense);
        // Packed elements are a keyless Vec<Property>: Empty remains a semantic hole, while a
        // live data slot can be decoded directly without an index string or entry-table chase.
        a.ldr_imm(14, 15, layout.vec_len_off as u32);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_HS, slow);
        a.ldr_imm(15, 15, layout.vec_ptr_off as u32);
        a.add_shifted(15, 15, 9, 4); // property_size == 16 (gate above)
        guard_prop_data(a, 14, 15, layout.property_meta as u32, slow);
        a.ldur(13, 15, layout.property_value as i32);
        a.mov_imm64(14, crate::value::PACK_EMPTY);
        a.cmp_reg_x(13, 14);
        a.b_cond(C_EQ, slow); // an absent own element must still consult the prototype chain
        emit_packed_word_decode(a, layout, slow);
        a.b(mirror_hit);
        a.bind(classic_dense);
    }
    a.ldr_imm(14, 12, evl);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; NO_SLOT (0xFFFF_FFFF) = hole → slow
    a.ldr_imm(12, 12, evp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base = entries data ptr + slot*entry_size
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. not an accessor
    guard_prop_data(a, 9, 15, ea, slow);
    // 9. Decode the heap's packed Value into the execution stack's wide `{tag,payload}` pair.
    // Numbers and objects dominate dense reads; test them first. BigInt remains checked because
    // its compound payload is not a one-word clone. Refcounted values clone by incrementing the
    // untagged stored pointer before the receiver's balancing decrement below.
    if layout.entry_accessor == layout.entry_value + 8 {
        emit_packed_entry_decode(a, layout, 15, slow);
    } else {
        a.ldurb(9, 15, ev);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.ldur(12, 15, ev);
        a.ldur(13, 15, ev + 8);
        let nobump = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nobump);
        a.ldur(16, 13, strong);
        a.add_imm(16, 16, 1);
        a.stur(16, 13, strong);
        a.bind(nobump);
    }
    // --- commit: everything validated; from here only writes ---
    a.bind(mirror_hit);
    // drop the receiver (strong was > 1; if the value IS the receiver the bump balanced it)
    a.ldur(9, 10, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 10, strong);
    // pop obj+key, push value → value lands at the obj slot, sp drops one
    a.stur(12, 20, -32);
    a.stur(13, 20, -24);
    a.sub_imm(20, 20, 16);
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
/// Where a mirror store's key index comes from.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
enum MirrorKey {
    /// Exact u32 already in an x register.
    U32InReg(u32),
    /// The key Value's f64 payload at `[x20 + off]` (already validated as an exact u32).
    StackF64(i32),
    /// A validated u32 key in a d register.
    F64InDreg(u32),
    /// Compile-time constant index.
    Const(u32),
}

/// What a mirror store writes.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
enum MirrorVal {
    /// A Value at `[x20 + off]` (tag at `off`, payload at `off+8`); tag unknown — a non-Num
    /// invalidates the mirror.
    Stack(i32),
    /// A proven-Num f64 in a d register; `bool` = proven exact-i32 (keeps `MIRROR_ALL_I32`).
    Num(u32, bool),
}

/// The element-mirror side of a dense element store the caller has already committed to the
/// entry (see `value::Props::mirror`): keep `mirror[n]` coherent, drop `MIRROR_ALL_I32` for
/// unproven values, and invalidate outright on a non-Num or the hole sentinel. Bounds are
/// re-checked against the mirror's own length as corruption insurance (the lockstep invariant
/// should make it redundant). Clobbers x9, x12, x13 and d1 only.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_mirror_store(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    base: u32,
    key: MirrorKey,
    val: MirrorVal,
) {
    let mf = (layout.obj_props + layout.props_mirror_flags) as u32;
    let mirror = (layout.obj_props + layout.props_elems) as u32;
    let mvp = (layout.dense_mirror + layout.vec_ptr_off) as u32;
    let mvl = (layout.dense_mirror + layout.vec_len_off) as u32;
    let done = a.new_label();
    let inval = a.new_label();
    a.ldrb_imm(13, base, mf);
    let ok_bit = asm::logical_imm_w(crate::value::MIRROR_OK as u32).unwrap();
    a.logic_imm_w(0, 12, 13, ok_bit);
    a.cbz(12, false, done);
    // Value → d1 (or reuse the proven register).
    let (dv, proven_num, proven_i32) = match val {
        MirrorVal::Stack(off) => {
            a.ldurb(9, 20, off);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, inval);
            a.ldur_d(1, 20, off + 8);
            (1u32, false, false)
        }
        MirrorVal::Num(d, i32_proven) => (d, true, i32_proven),
    };
    let _ = proven_num;
    if !proven_i32 {
        // MIRROR_ALL_I32 upkeep, flag-first: float-heavy code (flag long cleared) pays two
        // instructions. No hole-sentinel screen: hole accounting is structural (see
        // `Props::mirror_sync`), a data value equal to the sentinel bits is just a NaN to JIT
        // readers, and Rust readers fall back to the authoritative entry.
        let i32_done = a.new_label();
        let i32_bit = asm::logical_imm_w(crate::value::MIRROR_ALL_I32 as u32).unwrap();
        a.logic_imm_w(0, 9, 13, i32_bit);
        a.cbz(9, false, i32_done);
        a.fcvtzs_w_d(9, dv);
        a.scvtf_d_w(1, 9);
        a.fmov_x_d(9, 1);
        a.fmov_x_d(12, dv);
        a.cmp_reg_x(9, 12);
        a.b_cond(C_EQ, i32_done);
        let clear = asm::logical_imm_w(!(crate::value::MIRROR_ALL_I32 as u32)).unwrap();
        a.logic_imm_w(0, 13, 13, clear);
        a.strb_imm(13, base, mf);
        a.bind(i32_done);
    }
    // Key index → x9.
    match key {
        MirrorKey::U32InReg(r) => {
            if r != 9 {
                a.mov(9, r);
            }
        }
        MirrorKey::StackF64(off) => {
            a.ldur_d(0, 20, off);
            a.fcvtzu_w_d(9, 0);
        }
        MirrorKey::F64InDreg(d) => a.fcvtzu_w_d(9, d),
        MirrorKey::Const(n) => a.mov_imm64(9, n as u64),
    }
    // Insurance bounds check, then the store.
    a.ldr_imm(12, base, mirror);
    a.cbz(12, true, inval);
    a.ldr_imm(13, 12, mvl);
    a.cmp_reg_x(9, 13);
    a.b_cond(C_HS, inval);
    a.ldr_imm(12, 12, mvp);
    a.add_shifted(12, 12, 9, 3);
    a.str_d_imm(dv, 12, 0);
    a.b(done);
    a.bind(inval);
    a.strb_imm(31, base, mf);
    a.bind(done);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    let el = (layout.obj_props + layout.props_elems) as u32;
    let evp = (layout.dense_elems + layout.vec_ptr_off) as u32;
    let evl = (layout.dense_elems + layout.vec_len_off) as u32;
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
    // 1. stack: [obj @ -48, key @ -32, v @ -16]
    a.ldurb(9, 20, -48);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, slow);
    a.ldurb(9, 20, -32);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, slow);
    if keep {
        // v is also the expression result: a BigInt can't clone inline.
        a.ldurb(9, 20, -16);
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
    }
    // 2. key must be exactly a u32
    a.ldur_d(0, 20, -24);
    a.fcvtzu_w_d(9, 0);
    a.ucvtf_d_w(1, 9);
    a.fcmp(0, 1);
    a.b_cond(C_NE, slow);
    // 3. receiver refcount > 1
    a.ldur(10, 20, -40);
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
    a.ldr_imm(12, 11, el);
    a.cbz(12, true, slow);
    a.ldr_imm(14, 12, evl);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; hole → slow
    a.ldr_imm(12, 12, evp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. data property, writable
    guard_prop_data(a, 9, 15, ea, slow);
    guard_prop_writable(a, 9, 15, ew, slow);
    // 9. old value: trivially droppable (tag ≤ 4), or refcounted with strong > 1 (inline dec);
    //    BigInt or a last reference → helper. An old value that IS the receiver (`a[0] === a`)
    //    also bails: its dec plus the receiver dec below would take the shared counter to 0
    //    without running the destructor. w9 = old drop marker, x12 = old payload.
    if layout.entry_accessor == layout.entry_value + 8 {
        emit_packed_number_drop_guard(a, layout, 15, slow);
    } else {
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
    }
    // --- commit ---
    // Move v into the entry; a refcounted payload transfers ownership without a clone.
    a.ldur(14, 20, -16);
    a.ldur(17, 20, -8);
    if layout.entry_accessor == layout.entry_value + 8 {
        emit_packed_stack_encode(a, -16, slow);
        a.stur(16, 15, ev);
    } else {
        a.stur(14, 15, ev);
        a.stur(17, 15, ev + 8);
    }
    // drop the old value (refcounted: strong was > 1, so this never frees)
    let no_old_dec = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, no_old_dec);
    a.ldur(13, 12, strong);
    a.sub_imm(13, 13, 1);
    a.stur(13, 12, strong);
    a.bind(no_old_dec);
    // Keep the element mirror coherent (x9/x12/x13/d0/d1 are dead here; v words in 14/17,
    // bases in 10/11/15 stay live).
    emit_mirror_store(
        a,
        layout,
        11,
        MirrorKey::StackF64(-24),
        MirrorVal::Stack(-16),
    );
    if keep {
        // v now lives in the slot AND stays on the stack as the result: one bump.
        a.ldurb(9, 20, -16);
        let nb = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nb);
        a.ldur(13, 17, strong);
        a.add_imm(13, 13, 1);
        a.stur(13, 17, strong);
        a.bind(nb);
    }
    // drop the receiver (strong was > 1)
    a.ldur(13, 10, strong);
    a.sub_imm(13, 13, 1);
    a.stur(13, 10, strong);
    if keep {
        // [obj, key, v] → [v]: the result lands at the obj slot
        a.ldur(14, 20, -16);
        a.stur(14, 20, -48);
        a.stur(17, 20, -40);
        a.sub_imm(20, 20, 32);
    } else {
        a.sub_imm(20, 20, 48);
    }
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, l_unwind);
    a.bind(done);
}

/// Which fused parameter-slot element op to emit.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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

/// Guard that a packed property's old value is a Number, whose overwrite needs no destructor.
/// Keeping this numeric-only makes the emitted template small; other packed values use the
/// checked helper. On success w9 is the zero old-drop marker and x12 is scratch.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_packed_number_drop_guard(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    entry: u32,
    slow: usize,
) {
    a.ldur(12, entry, layout.entry_value as i32);
    a.lsr_imm(9, 12, 48);
    // PACK_OBJ sorts above the tagged scalar range, so reject it before the two numeric ranges.
    a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_EQ, slow);
    let number = a.new_label();
    a.movz(14, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_LO, number);
    a.movz(14, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_LS, slow);
    a.bind(number);
    a.movz(9, 0, 0);
}

/// Encode a wide Number at `off` into x16. Other kinds stay on the checked path, keeping the
/// per-site packed-write template compact; Number payload bits are already NaN-box compatible.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_packed_stack_encode(a: &mut asm::Asm, off: i32, slow: usize) {
    a.ldurb(13, 20, off);
    a.cmp_imm_w(13, 4);
    a.b_cond(C_NE, slow);
    a.ldur(16, 20, off + 8);
    // Any NaN payload could overlap a boxed tag. Match PackedValue::pack by canonicalizing it.
    a.ldur_d(1, 20, off + 8);
    a.fcmp(1, 1);
    let nan = a.new_label();
    let encoded = a.new_label();
    a.b_cond(C_VS, nan);
    a.b(encoded);
    a.bind(nan);
    a.mov_imm64(16, crate::value::PACK_CANON_NAN);
    a.bind(encoded);
}

/// Encode any wide local-slot value except BigInt into x16, transferring the stack value's
/// ownership when the caller commits. All exits to `slow` happen before frame mutation.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_packed_stack_encode_all(a: &mut asm::Asm, off: i32, slow: usize) {
    a.ldurb(9, 20, off);
    a.ldur(16, 20, off + 8);
    let done = a.new_label();
    let number = a.new_label();
    let undefined = a.new_label();
    let empty = a.new_label();
    let null = a.new_label();
    let boolean = a.new_label();
    let string = a.new_label();
    let symbol = a.new_label();
    let object = a.new_label();
    // Number and Object dominate local writes; keep both at the front of the dispatch chain.
    for (tag, label) in [
        (4, number),
        (8, object),
        (0, undefined),
        (3, boolean),
        (6, string),
        (7, symbol),
        (1, empty),
        (2, null),
    ] {
        a.cmp_imm_w(9, tag);
        a.b_cond(C_EQ, label);
    }
    a.b(slow); // BigInt (5), or a corrupt/unknown discriminant.
    for (label, bits) in [
        (undefined, crate::value::PACK_UNDEFINED),
        (empty, crate::value::PACK_EMPTY),
        (null, crate::value::PACK_NULL),
    ] {
        a.bind(label);
        a.mov_imm64(16, bits);
        a.b(done);
    }
    a.bind(boolean);
    a.ldurb(16, 20, off + 1);
    a.mov_imm64(14, crate::value::PACK_BOOL);
    a.logic_x(1, 16, 16, 14);
    a.b(done);
    for (label, bits) in [
        (string, crate::value::PACK_STR),
        (symbol, crate::value::PACK_SYM),
        (object, crate::value::PACK_OBJ),
    ] {
        a.bind(label);
        a.mov_imm64(14, bits);
        a.logic_x(1, 16, 16, 14);
        a.b(done);
    }
    a.bind(number);
    a.ldur_d(1, 20, off + 8);
    a.fcmp(1, 1);
    let number_nan = a.new_label();
    let number_ok = a.new_label();
    a.b_cond(C_VS, number_nan);
    a.b(number_ok);
    a.bind(number_nan);
    a.mov_imm64(16, crate::value::PACK_CANON_NAN);
    a.bind(number_ok);
    a.bind(done);
}

/// Decode one NaN-boxed heap property into the execution stack's wide x12/x13 Value pair.
/// `entry` points at `(Rc<str>, Property)` and all guards branch to `slow` before mutation.
/// BigInt stays checked; strings, symbols and objects clone by incrementing their Rc count.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_packed_entry_decode(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    entry: u32,
    slow: usize,
) {
    let ev = layout.entry_value as i32;
    a.ldur(13, entry, ev);
    emit_packed_word_decode(a, layout, slow);
}

/// Decode the packed word in x13 into the wide execution pair x12/x13, cloning any shared
/// reference payload. BigInt remains on the checked path.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_packed_word_decode(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    slow: usize,
) {
    let strong = layout.rc_strong_off as i32;
    a.lsr_imm(9, 13, 48);
    let decoded = a.new_label();
    let is_number = a.new_label();
    let is_obj = a.new_label();
    let is_str = a.new_label();
    let is_sym = a.new_label();
    let is_bool = a.new_label();
    let is_undefined = a.new_label();
    let is_empty = a.new_label();
    let is_null = a.new_label();
    a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_EQ, is_obj);
    a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_LO, is_number);
    a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_HI, is_number);
    for (tag, label) in [
        (crate::value::PACK_UNDEFINED, is_undefined),
        (crate::value::PACK_EMPTY, is_empty),
        (crate::value::PACK_NULL, is_null),
        (crate::value::PACK_BOOL, is_bool),
        (crate::value::PACK_STR, is_str),
        (crate::value::PACK_SYM, is_sym),
        (crate::value::PACK_BIGINT, slow),
    ] {
        a.movz(16, (tag >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_EQ, label);
    }
    a.b(slow);
    a.bind(is_number);
    a.movz(12, 4, 0);
    a.b(decoded);
    for (label, tag) in [(is_undefined, 0), (is_empty, 1), (is_null, 2)] {
        a.bind(label);
        a.movz(12, tag, 0);
        a.movz(13, 0, 0);
        a.b(decoded);
    }
    a.bind(is_bool);
    a.movz(12, 3, 0);
    a.lsl_imm_w(13, 13, 8);
    a.logic_w(1, 12, 12, 13);
    a.movz(13, 0, 0);
    a.b(decoded);
    for (label, tag) in [(is_str, 6), (is_sym, 7), (is_obj, 8)] {
        a.bind(label);
        a.movz(12, tag, 0);
        a.lsl_imm(13, 13, 16);
        a.lsr_imm(13, 13, 16);
        a.ldur(16, 13, strong);
        a.add_imm(16, 16, 1);
        a.stur(16, 13, strong);
        a.b(decoded);
    }
    a.bind(decoded);
}

/// Inline fused element access where the receiver lives in a *parameter* slot
/// ([`crate::bytecode::Op::GetElemLocal`] and friends): like [`emit_get_elem_inline`] /
/// [`emit_set_elem_inline`] but the receiver is read straight out of the slot — it never crosses
/// the operand stack, so there is no receiver clone/drop refcounting at all (the slot's own
/// reference keeps it alive; no user code runs inside the fast path). A non-Obj slot (including
/// a defensive TDZ Empty) falls to the checked helper, which re-runs the op generically.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    let el = (layout.obj_props + layout.props_elems) as u32;
    let evp = (layout.dense_elems + layout.vec_ptr_off) as u32;
    let evl = (layout.dense_elems + layout.vec_len_off) as u32;
    let mvp = (layout.dense_mirror + layout.vec_ptr_off) as u32;
    let mvl = (layout.dense_mirror + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;
    let get = kind == ElemLocalKind::Get;
    debug_assert!(get || key == KeySrc::Stack);
    // Stack-keyed layout: Get → [key @ -16]; Set* → [key @ -32, v @ -16].
    let key_off = if get { -16 } else { -32 };

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
    let mirror_hit = a.new_label();
    let classic = a.new_label();
    if get {
        // 5. mirror read: bounds + one indexed load of a known Num (see emit_get_elem_inline).
        let mf = (layout.obj_props + layout.props_mirror_flags) as u32;
        let mirror = el;
        a.ldrb_imm(12, 11, mf);
        let mask =
            asm::logical_imm_w((crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32)
                .unwrap();
        a.logic_imm_w(0, 12, 12, mask);
        a.cmp_imm_w(
            12,
            (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
        );
        a.b_cond(C_NE, classic);
        a.ldr_imm(12, 11, mirror);
        a.cbz(12, true, classic);
        a.ldr_imm(14, 12, mvl);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_HS, classic);
        a.ldr_imm(12, 12, mvp);
        a.ldr_d_lsl3(1, 12, 9);
        a.movz(12, 4, 0);
        a.fmov_x_d(13, 1);
        a.movz(14, 0, 0);
        a.b(mirror_hit);
    }
    a.bind(classic);
    // 5b. dense bounds
    a.ldr_imm(12, 11, el);
    a.cbz(12, true, slow);
    if get && packed_elem_inlinable(layout) {
        let classic_dense = a.new_label();
        a.ldr_imm(15, 12, layout.dense_packed as u32);
        a.cbz(15, true, classic_dense);
        a.ldr_imm(14, 15, layout.vec_len_off as u32);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_HS, slow);
        a.ldr_imm(15, 15, layout.vec_ptr_off as u32);
        a.add_shifted(15, 15, 9, 4);
        guard_prop_data(a, 14, 15, layout.property_meta as u32, slow);
        a.ldur(13, 15, layout.property_value as i32);
        a.mov_imm64(14, crate::value::PACK_EMPTY);
        a.cmp_reg_x(13, 14);
        a.b_cond(C_EQ, slow);
        emit_packed_word_decode(a, layout, slow);
        a.b(mirror_hit);
        a.bind(classic_dense);
    }
    a.ldr_imm(14, 12, evl);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, slow);
    // 6. slot = elems[n]; hole → slow
    a.ldr_imm(12, 12, evp);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, slow);
    // 7. entry base
    a.ldr_imm(15, 11, en);
    a.mov_imm64(16, es);
    a.madd(15, 13, 16, 15);
    // 8. data property (+ writable for the set forms)
    guard_prop_data(a, 9, 15, ea, slow);
    if get {
        // 9. clone the element into the execution stack representation.
        if layout.entry_accessor == layout.entry_value + 8 {
            emit_packed_entry_decode(a, layout, 15, slow);
        } else {
            a.ldurb(9, 15, ev);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_EQ, slow);
            a.ldur(12, 15, ev);
            a.ldur(13, 15, ev + 8);
            let nobump = a.new_label();
            a.cmp_imm_w(9, 6);
            a.b_cond(C_LO, nobump);
            a.ldur(16, 13, strong);
            a.add_imm(16, 16, 1);
            a.stur(16, 13, strong);
            a.bind(nobump);
        }
        a.bind(mirror_hit);
        match key {
            KeySrc::Stack => {
                // pop key, push value → result replaces the key slot
                a.stur(12, 20, -16);
                a.stur(13, 20, -8);
            }
            KeySrc::Slot(_) | KeySrc::SlotPre(..) => {
                if let KeySrc::SlotPre(k_off, _) = key {
                    a.str_d_imm(0, 22, k_off + 8); // commit the deferred ±1 to the slot
                }
                // nothing was on the stack: push the value
                a.stur(12, 20, 0);
                a.stur(13, 20, 8);
                a.add_imm(20, 20, 16);
            }
        }
    } else {
        guard_prop_writable(a, 9, 15, ew, slow);
        if kind == ElemLocalKind::SetKeep {
            // v is also the expression result: a BigInt can't clone inline.
            a.ldurb(9, 20, -16);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_EQ, slow);
        }
        // 9. old value: trivially droppable, or refcounted with strong > 1.
        if layout.entry_accessor == layout.entry_value + 8 {
            emit_packed_number_drop_guard(a, layout, 15, slow);
        } else {
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
        }
        // --- commit: move v into the entry, drop the old value ---
        a.ldur(14, 20, -16);
        a.ldur(17, 20, -8);
        if layout.entry_accessor == layout.entry_value + 8 {
            emit_packed_stack_encode(a, -16, slow);
            a.stur(16, 15, ev);
        } else {
            a.stur(14, 15, ev);
            a.stur(17, 15, ev + 8);
        }
        let no_old_dec = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, no_old_dec);
        a.ldur(13, 12, strong);
        a.sub_imm(13, 13, 1);
        a.stur(13, 12, strong);
        a.bind(no_old_dec);
        // Element mirror (x9/x12/x13/d1 dead here; the key f64 survives in d0).
        emit_mirror_store(
            a,
            layout,
            11,
            MirrorKey::F64InDreg(0),
            MirrorVal::Stack(-16),
        );
        if kind == ElemLocalKind::SetKeep {
            // v now lives in the slot AND stays on the stack: one bump, result at the key slot.
            a.ldurb(9, 20, -16);
            let nb = a.new_label();
            a.cmp_imm_w(9, 6);
            a.b_cond(C_LO, nb);
            a.ldur(13, 17, strong);
            a.add_imm(13, 13, 1);
            a.stur(13, 17, strong);
            a.bind(nb);
            a.ldur(14, 20, -16);
            a.stur(14, 20, -32);
            a.stur(17, 20, -24);
            a.sub_imm(20, 20, 16);
        } else {
            a.sub_imm(20, 20, 32);
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    /// Monomorphic shape-validated property read whose live value must be a Num. `u32::MAX`
    /// denotes the frame's `this` binding; every other value is a local-slot byte offset.
    /// The IC state is baked only as a guarded lookup recipe: live shapes, bounds, attributes,
    /// and the value tag are still checked on every execution.
    LoadProp(u32, crate::bytecode::IcState),
    /// Own writable numeric property store. Receiver encoding matches `LoadProp`; the virtual
    /// top is consumed and written without materializing a wide stack Value.
    StoreProp(u32, crate::bytecode::IcState),
    /// Terminal fused compare+branch: negated ARM condition + target pc.
    CmpBranch(u32, usize),
}

/// First CFG-region lowering: a helper-free numeric loop with one forward diamond.  The shape is
/// common in scheduler/worker kernels: advance a numeric `this` field, optionally wrap it, and
/// fill a dense numeric array.  All mutable state has fixed register homes across both arms; the
/// shared [`crate::jit_ir`] graph proves the loop and its block boundaries, while the baseline JIT
/// remains the exact-PC side-exit target.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct NumericDiamondPlan {
    head: usize,
    exit_pc: usize,
    index_off: u32,
    owner_off: u32,
    limit_cache: usize,
    counter: crate::bytecode::IcState,
    array_prop: crate::bytecode::IcState,
    threshold: i64,
    reset: i64,
}

/// Hot prefix of Richards-style schedulers: inspect a linked task-control block's numeric state,
/// skip held/suspended nodes, and side-exit to the active-task body. Unlike a benchmark-name
/// intrinsic, discovery is entirely structural and every cached shape, method identity, global
/// number, ownership count, and property descriptor is guarded live.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerShellPlan {
    head: usize,
    active_pc: usize,
    null_pc: usize,
    temp_off: u32,
    current: crate::bytecode::IcState,
    state: crate::bytecode::IcState,
    link: crate::bytecode::IcState,
    method: crate::bytecode::IcState,
    method_expected: usize,
    held_cache: usize,
    suspended_cache: usize,
    active: Option<SchedulerActivePlan>,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerActivePlan {
    exit_pc: usize,
    tcb_off: u32,
    packet_off: u32,
    id: crate::bytecode::IcState,
    state: crate::bytecode::IcState,
    current_id: crate::bytecode::IcState,
    run_method: crate::bytecode::IcState,
    run_expected: usize,
    queue: crate::bytecode::IcState,
    packet_link: crate::bytecode::IcState,
    suspended_runnable_cache: usize,
    running_cache: usize,
    runnable_cache: usize,
    suspended_runnable: i64,
    running: i64,
    runnable: i64,
    null_dispatch: Option<SchedulerActiveNullDispatchPlan>,
}

/// Preplanned null-packet continuation from SchedulerActive into the two task classifiers that
/// dominate Richards. The active emitter keeps the TCB virtual in x0 and leaves scalar sentinels
/// in the frame; any total miss materializes the exact pc59 snapshot before ordinary bytecode.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerActiveNullDispatchPlan {
    device: SchedulerDevicePlan,
    handler: SchedulerHandlerSuspendPlan,
    handler_incoming_suspend: bool,
    handler_incoming_work_delivery: bool,
    idle: Option<SchedulerActiveIdlePlan>,
    worker: Option<SchedulerActiveWorkerPlan>,
}

/// Cross-child IdleTask transaction selected from the scheduler's polymorphic run call cache.
/// `run_expected` pins the exact method identity at runtime before any cache pointer from the
/// child Idle chunk is touched.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerActiveIdlePlan {
    task: crate::bytecode::IcState,
    run_method: crate::bytecode::IcState,
    run_expected: usize,
    release: SchedulerIdleReleasePlan,
}

/// Cross-child WorkerTask arms discovered through the scheduler's polymorphic run profile. The
/// null arm is a guarded bridge into suspend; the packet arm is a complete Worker/queue/preempt
/// transaction and therefore never constructs the two nested JS call frames on success.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerActiveWorkerPlan {
    task: crate::bytecode::IcState,
    run_method: crate::bytecode::IcState,
    run_expected: usize,
    suspend: SchedulerDeviceSuspendPlan,
    work: Option<SchedulerActiveWorkerWorkPlan>,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerActiveWorkerWorkPlan {
    v1: crate::bytecode::IcState,
    v2: crate::bytecode::IcState,
    packet_a1: crate::bytecode::IcState,
    packet_a2: crate::bytecode::IcState,
    id_a_cache: usize,
    id_a_else_cache: usize,
    id_b_cache: usize,
    data_size_cache: usize,
    threshold: i64,
    reset: i64,
    queue: SchedulerDeviceQueuePlan,
}

/// Whole-function fast path for Richards' IdleTask release arm. The ordinary bytecode remains
/// the exact replay path for the final hold, observable property access, coercions, missing
/// targets, and non-preempting releases; the specialized arm is entirely guard-then-commit.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerIdleReleasePlan {
    count: crate::bytecode::IcState,
    v1: crate::bytecode::IcState,
    scheduler: crate::bytecode::IcState,
    release_method: crate::bytecode::IcState,
    release_expected: usize,
    id_a_cache: usize,
    id_b_cache: usize,
    blocks: crate::bytecode::IcState,
    mark_method: crate::bytecode::IcState,
    mark_expected: usize,
    state: crate::bytecode::IcState,
    not_held_cache: usize,
    target_priority: crate::bytecode::IcState,
    current: crate::bytecode::IcState,
    current_priority: crate::bytecode::IcState,
}

/// Guarded bridge from the scheduler's polymorphic task call into the already-inlined
/// DeviceTask body. It removes the generic property/method dispatch and materializes the three
/// compiler locals once, then resumes at the exact suspend, queue, or hold arm.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerDevicePlan {
    suspend_pc: usize,
    queue_pc: usize,
    hold_pc: usize,
    tcb_off: u32,
    packet_off: u32,
    device_packet_off: u32,
    task_off: u32,
    temp_off: u32,
    task: crate::bytecode::IcState,
    run_method: crate::bytecode::IcState,
    run_expected: usize,
    v1: crate::bytecode::IcState,
    suspend: Option<SchedulerDeviceSuspendPlan>,
    queue: Option<SchedulerDeviceQueuePlan>,
    hold: Option<SchedulerDeviceHoldPlan>,
}

/// The dominant HandlerTask arm needs no packet work: both the incoming packet and Handler.v1
/// are exactly Null, so the inlined body immediately calls `scheduler.suspendCurrent()`. This
/// compact plan proves that path and reuses the same flattened suspend transaction as DeviceTask.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerHandlerIncomingPlan {
    kind: crate::bytecode::IcState,
    kind_work_cache: usize,
    add_method: crate::bytecode::IcState,
    add_expected: usize,
    packet_link: crate::bytecode::IcState,
    delivery: SchedulerHandlerDeliverPlan,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerHandlerSuspendPlan {
    tcb_off: u32,
    packet_off: u32,
    completion_pc: usize,
    delivery_pc: usize,
    task: crate::bytecode::IcState,
    run_method: crate::bytecode::IcState,
    run_expected: usize,
    v1: crate::bytecode::IcState,
    v2: crate::bytecode::IcState,
    packet_a1: crate::bytecode::IcState,
    data_size_cache: usize,
    suspend: SchedulerDeviceSuspendPlan,
    incoming: Option<SchedulerHandlerIncomingPlan>,
    null_full: Option<SchedulerHandlerNullFullPlan>,
}

/// The two remaining exact-Null incoming-packet arms in HandlerTask.run. Both begin from the
/// Active dispatcher with the task and current TCB still virtual: an unfinished work packet can
/// deliver Handler.v2, while a completed one transfers Handler.v1 through Scheduler.queue.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerHandlerNullFullPlan {
    delivery: SchedulerHandlerDeliverPlan,
    queue: SchedulerHandlerQueuePlan,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerHandlerQueuePlan {
    handler_off: u32,
    v1: crate::bytecode::IcState,
    queue: SchedulerDeviceQueuePlan,
}

/// HandlerTask's high-volume delivery arm moves one packet from v2, copies one numeric payload
/// cell from v1, and queues the packet onto either an empty or one-node target worklist. The
/// source/successor and appended packet ownership moves are count-neutral; an empty preempting
/// target additionally performs one guarded current-TCB owner replacement.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerHandlerDeliverPlan {
    handler_off: u32,
    count_off: u32,
    saved_off: u32,
    loop_pc: usize,
    empty_preempt: bool,
    v1: crate::bytecode::IcState,
    v2: crate::bytecode::IcState,
    payload_array: crate::bytecode::IcState,
    packet_a1: crate::bytecode::IcState,
    work_a1: crate::bytecode::IcState,
    scheduler: crate::bytecode::IcState,
    queue_method: crate::bytecode::IcState,
    queue_expected: usize,
    blocks: crate::bytecode::IcState,
    packet_id: crate::bytecode::IcState,
    queue_count: crate::bytecode::IcState,
    packet_link: crate::bytecode::IcState,
    current_id: crate::bytecode::IcState,
    check_method: crate::bytecode::IcState,
    check_expected: usize,
    current: crate::bytecode::IcState,
    target_queue: crate::bytecode::IcState,
    mark_method: crate::bytecode::IcState,
    mark_expected: usize,
    state: crate::bytecode::IcState,
    runnable_cache: usize,
    target_priority: crate::bytecode::IcState,
    current_priority: crate::bytecode::IcState,
    add_method: crate::bytecode::IcState,
    add_expected: usize,
    queued_link: crate::bytecode::IcState,
}

/// Select where the Handler delivery transaction obtains its already-observed inputs. Both
/// stitched forms enter with x2=count, x3=Handler.v2 entry, x5=Handler, and x6=packet. The
/// incoming-device form additionally has x15=packet.link entry already proven exact Null.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
enum SchedulerHandlerDeliverSource {
    Locals,
    ActiveNull,
    IncomingDevice,
    ActiveIncomingWork,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
enum SchedulerQueueSource {
    Clear,
    AdvanceToPacketLink,
}

// Stack-resident, non-owning prototype cache for one bounded direct Scheduler session. The four
// words occupy the scheduler-only gap between saved x23..x28 and d8..d15; no runtime object is
// retained after the native frame returns, and every session entry clears the complete cache.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_ROLE_DEVICE_PROTO_SP: u32 = 96;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_ROLE_HANDLER_PROTO_SP: u32 = 104;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_ROLE_IDLE_PROTO_SP: u32 = 112;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_ROLE_WORKER_PROTO_SP: u32 = 120;

// A graph-compatible scheduler gets a 512-byte native frame. The graph cache is deliberately
// raw and non-owning: Scheduler.blocks owns every cached TCB, each TCB owns its task, and x28 is
// published only for a bounded helper-free/user-code-free session. Existing role-prototype words
// remain at 96..128; the graph header occupies 128..160, six 48-byte records occupy 160..448,
// and d8..d15 are saved at 448..512. The header's second word is a soft contract bitmap: a failed
// optional proof leaves it zero while the already-valid graph epoch continues unchanged.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_CURRENT_ID_ENTRY_SP: u32 = 128;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_CORE_FLAGS_SP: u32 = 136;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_CORE_VALID: u32 = 1;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_HELD_SP: u32 = 144;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_SUSPENDED_SP: u32 = 152;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_RECORDS_SP: u32 = 160;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_RECORD_SIZE: u32 = 48;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_RECORD_COUNT: u32 = 6;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_TCB_OFF: u32 = 0;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_TASK_OFF: u32 = 8;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_STATE_ENTRY_OFF: u32 = 16;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_QUEUE_ENTRY_OFF: u32 = 24;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_LINK_RECORD_OFF: u32 = 32;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const SCHED_GRAPH_ID_BITS_OFF: u32 = 40;

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const fn scheduler_graph_record_sp(index: u32) -> u32 {
    SCHED_GRAPH_RECORDS_SP + index * SCHED_GRAPH_RECORD_SIZE
}

/// A caller-established route to one exact graph record. `sp_bias` keeps the contract header
/// address explicit for future nested consumers; the first Active-null consumer runs after its
/// 48-byte snapshot has been restored and therefore uses the scheduler-frame base directly.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
struct SchedulerGraphCoreContext {
    current_record: u32,
    sp_bias: u32,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
struct SchedulerDeviceSuspendPlan {
    loop_pc: usize,
    scheduler: crate::bytecode::IcState,
    suspend_method: crate::bytecode::IcState,
    suspend_expected: usize,
    current: crate::bytecode::IcState,
    mark_method: crate::bytecode::IcState,
    mark_expected: usize,
    state: crate::bytecode::IcState,
    suspended_cache: usize,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerDeviceHoldPlan {
    loop_pc: usize,
    scheduler: crate::bytecode::IcState,
    hold_method: crate::bytecode::IcState,
    hold_expected: usize,
    hold_count: crate::bytecode::IcState,
    current: crate::bytecode::IcState,
    mark_method: crate::bytecode::IcState,
    mark_expected: usize,
    state: crate::bytecode::IcState,
    held_cache: usize,
    link: crate::bytecode::IcState,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
struct SchedulerDeviceQueuePlan {
    loop_pc: usize,
    scheduler: crate::bytecode::IcState,
    queue_method: crate::bytecode::IcState,
    queue_expected: usize,
    blocks: crate::bytecode::IcState,
    packet_id: crate::bytecode::IcState,
    queue_count: crate::bytecode::IcState,
    packet_link: crate::bytecode::IcState,
    current_id: crate::bytecode::IcState,
    check_method: crate::bytecode::IcState,
    check_expected: usize,
    current: crate::bytecode::IcState,
    target_queue: crate::bytecode::IcState,
    mark_method: crate::bytecode::IcState,
    mark_expected: usize,
    state: crate::bytecode::IcState,
    runnable_cache: usize,
    target_priority: crate::bytecode::IcState,
    current_priority: crate::bytecode::IcState,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct LinkedScanPlan {
    exit_pc: usize,
    next_off: u32,
    peek_off: u32,
    link: crate::bytecode::IcState,
    loose_null_compare: bool,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct SchedulerIdleReleaseArm {
    scheduler_name: u32,
    scheduler_cache: u32,
    release_name: u32,
    release_method_cache: u32,
    id_cache: u32,
    release_target: u32,
    blocks_name: u32,
    blocks_cache: u32,
    mark_name: u32,
    mark_method_cache: u32,
    mark_target: u32,
    state_name: u32,
    state_cache: u32,
    state_store: u32,
    not_held_name: u32,
    not_held_cache: u32,
    priority_name: u32,
    target_priority_cache: u32,
    current_name: u32,
    current_cache: u32,
    current_priority_cache: u32,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn parse_scheduler_idle_release_arm(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    start: usize,
) -> Option<SchedulerIdleReleaseArm> {
    use crate::bytecode::Op;
    let end = start.checked_add(45)?;
    let [
        Op::GetPropThis(scheduler_name, scheduler_cache),
        Op::GetMethod(release_name, release_method_cache),
        Op::LoadName(_, id_cache),
        Op::InlineGuard(release_target, release_generic),
        Op::StoreLocal(id),
        Op::Pop,
        Op::StoreLocal(scheduler),
        Op::ResetSlots(target, reset_count),
        Op::GetPropLocal(scheduler0, blocks_name, blocks_cache),
        Op::LoadLocal(id0),
        Op::GetElem,
        Op::StoreLocal(target0),
        Op::LoadLocal(target1),
        Op::Const(null_target),
        Op::EqEq,
        Op::JumpIfFalse(nonnull_target),
        Op::LoadLocal(target2),
        Op::Jump(null_return),
        Op::LoadLocal(target3),
        Op::GetMethod(mark_name, mark_method_cache),
        Op::InlineGuard(mark_target, mark_generic),
        Op::Pop,
        Op::StoreLocal(mark_this),
        Op::GetPropLocal(mark_this0, state_name, state_cache),
        Op::LoadName(not_held_name, not_held_cache),
        Op::BitAnd,
        Op::SetPropLocalDrop(mark_this1, state_name1, state_store),
        Op::Undef,
        Op::Jump(mark_join),
        Op::CallWithThis(0, _),
        Op::Pop,
        Op::GetPropLocal(target4, priority_name, target_priority_cache),
        Op::GetPropLocal(scheduler1, current_name, current_cache),
        Op::GetProp(priority_name1, current_priority_cache),
        Op::Gt,
        Op::JumpIfFalse(no_preempt),
        Op::LoadLocal(target5),
        Op::Jump(return_join0),
        Op::Jump(return_current),
        Op::GetPropLocal(scheduler2, current_name1, _current_cache_again),
        Op::Jump(return_join1),
        Op::Undef,
        Op::Jump(return_join2),
        Op::CallWithThis(1, _),
        Op::Return,
    ] = ops.get(start..end)?
    else {
        return None;
    };
    if id != id0
        || scheduler != scheduler0
        || scheduler != scheduler1
        || scheduler != scheduler2
        || target != target0
        || target != target1
        || target != target2
        || target != target3
        || target != target4
        || target != target5
        || mark_this != mark_this0
        || mark_this != mark_this1
        || state_name != state_name1
        || priority_name != priority_name1
        || current_name != current_name1
        || *reset_count != 1
        || *release_generic as usize != start + 43
        || *nonnull_target as usize != start + 18
        || *null_return as usize != start + 44
        || *mark_generic as usize != start + 29
        || *mark_join as usize != start + 30
        || *no_preempt as usize != start + 39
        || *return_join0 as usize != start + 44
        || *return_current as usize != start + 41
        || *return_join1 as usize != start + 44
        || *return_join2 as usize != start + 44
        || !chunk.jit_const_copyable(*null_target)
        || chunk.jit_const_bits(*null_target) != (2, 0)
    {
        return None;
    }
    Some(SchedulerIdleReleaseArm {
        scheduler_name: *scheduler_name,
        scheduler_cache: *scheduler_cache,
        release_name: *release_name,
        release_method_cache: *release_method_cache,
        id_cache: *id_cache,
        release_target: *release_target,
        blocks_name: *blocks_name,
        blocks_cache: *blocks_cache,
        mark_name: *mark_name,
        mark_method_cache: *mark_method_cache,
        mark_target: *mark_target,
        state_name: *state_name,
        state_cache: *state_cache,
        state_store: *state_store,
        not_held_name: *not_held_name,
        not_held_cache: *not_held_cache,
        priority_name: *priority_name,
        target_priority_cache: *target_priority_cache,
        current_name: *current_name,
        current_cache: *current_cache,
        current_priority_cache: *current_priority_cache,
    })
}

/// Recognize the complete 118-op IdleTask body. Only the hot `count > 1` release arm is
/// specialized: all observable operations are represented by live guards and every mutation is
/// delayed until those guards prove the canonical higher-priority-target outcome.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_idle_release(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerIdleReleasePlan> {
    use crate::bytecode::{Op, UpdKind};
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_IDLE_RELEASE").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !set_prop_inlinable(layout)
        || !get_method_inlinable(layout)
        || !elem_inlinable(layout)
        || !packed_elem_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
        || cfg.stack_depth_at(0) != Some(0)
        || ops.len() != 118
    {
        return None;
    }
    let [
        Op::LoadThis,
        Op::UpdateProp(count_name0, count_cache0, UpdKind::DecDiscard),
        Op::GetPropThis(count_name1, count_cache1),
        Op::Const(count_zero),
        Op::EqEq,
        Op::JumpIfFalse(release_pc),
        Op::GetPropThis(_, _),
        Op::GetMethod(_, _),
        Op::CallWithThis(0, _),
        Op::Return,
        Op::GetPropThis(v1_name0, v1_cache0),
        Op::Const(bit_one),
        Op::BitAnd,
        Op::Const(bit_zero),
        Op::EqEq,
        Op::JumpIfFalse(odd_pc),
        Op::GetPropThis(v1_name1, v1_cache1),
        Op::Const(shift_one0),
        Op::Shr,
        Op::SetPropThisDrop(v1_name2, v1_store0),
    ] = &ops[..20]
    else {
        return None;
    };
    let [
        Op::Jump(final_return),
        Op::GetPropThis(v1_name3, v1_cache2),
        Op::Const(shift_one1),
        Op::Shr,
        Op::Const(xor_mask),
        Op::BitXor,
        Op::SetPropThisDrop(v1_name4, v1_store1),
    ] = &ops[65..72]
    else {
        return None;
    };
    if count_name0 != count_name1
        || v1_name0 != v1_name1
        || v1_name0 != v1_name2
        || v1_name0 != v1_name3
        || v1_name0 != v1_name4
        || *release_pc != 10
        || *odd_pc != 66
        || *final_return != 117
        || !matches!(ops[117], Op::ReturnUndef)
    {
        return None;
    }
    let int_const = |index: u32| {
        if !chunk.jit_const_copyable(index) {
            return None;
        }
        let (tag, bits) = chunk.jit_const_bits(index);
        (tag == 4).then(|| exact_i32_const(bits)).flatten()
    };
    if int_const(*count_zero)? != 0
        || int_const(*bit_one)? != 1
        || int_const(*bit_zero)? != 0
        || int_const(*shift_one0)? != 1
        || int_const(*shift_one1)? != 1
        || int_const(*xor_mask)? != 0xD008
    {
        return None;
    }

    let a = parse_scheduler_idle_release_arm(chunk, ops, 20)?;
    let b = parse_scheduler_idle_release_arm(chunk, ops, 72)?;
    if a.scheduler_name != b.scheduler_name
        || a.release_name != b.release_name
        || a.blocks_name != b.blocks_name
        || a.mark_name != b.mark_name
        || a.state_name != b.state_name
        || a.not_held_name != b.not_held_name
        || a.priority_name != b.priority_name
        || a.current_name != b.current_name
    {
        return None;
    }

    let count = chunk.jit_cache_preferred(*count_cache0)?;
    let v1 = chunk.jit_cache_preferred(*v1_cache0)?;
    let scheduler = chunk.jit_cache_preferred(a.scheduler_cache)?;
    let release_method = chunk.jit_cache_preferred(a.release_method_cache)?;
    let blocks = chunk.jit_cache_preferred(a.blocks_cache)?;
    let mark_method = chunk.jit_cache_preferred(a.mark_method_cache)?;
    let state = chunk.jit_cache_preferred(a.state_cache)?;
    let target_priority = chunk.jit_cache_preferred(a.target_priority_cache)?;
    let current = chunk.jit_cache_preferred(a.current_cache)?;
    let current_priority = chunk.jit_cache_preferred(a.current_priority_cache)?;
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let same_ic = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == expected.depth
                && actual.recv_shape == expected.recv_shape
                && actual.holder_shape == expected.holder_shape
                && actual.slot == expected.slot
        })
    };
    let idle_shape = count.recv_shape;
    let scheduler_shape = blocks.recv_shape;
    let tcb_shape = state.recv_shape;
    if count.depth != 0
        || !same_own(*count_cache1, count)
        || v1.depth != 0
        || v1.recv_shape != idle_shape
        || v1.slot == count.slot
        || !same_own(*v1_cache1, v1)
        || !same_own(*v1_store0, v1)
        || !same_own(*v1_cache2, v1)
        || !same_own(*v1_store1, v1)
        || scheduler.depth != 0
        || scheduler.recv_shape != idle_shape
        || scheduler.slot == count.slot
        || scheduler.slot == v1.slot
        || !same_own(b.scheduler_cache, scheduler)
        || release_method.depth != 1
        || release_method.recv_shape != scheduler_shape
        || !same_ic(b.release_method_cache, release_method)
        || blocks.depth != 0
        || !same_own(b.blocks_cache, blocks)
        || current.depth != 0
        || current.recv_shape != scheduler_shape
        || current.slot == blocks.slot
        || !same_own(b.current_cache, current)
        || mark_method.depth != 1
        || mark_method.recv_shape != tcb_shape
        || !same_ic(b.mark_method_cache, mark_method)
        || state.depth != 0
        || !same_own(a.state_store, state)
        || !same_own(b.state_cache, state)
        || !same_own(b.state_store, state)
        || target_priority.depth != 0
        || target_priority.recv_shape != tcb_shape
        || target_priority.slot == state.slot
        || !same_own(b.target_priority_cache, target_priority)
        || current_priority.depth != 0
        || current_priority.recv_shape != tcb_shape
        || current_priority.slot != target_priority.slot
        || !same_own(b.current_priority_cache, current_priority)
        || idle_shape == scheduler_shape
        || idle_shape == tcb_shape
        || scheduler_shape == tcb_shape
        || chunk
            .jit_name_number(a.id_cache)
            .and_then(exact_i32_const)
            .is_none()
        || chunk
            .jit_name_number(b.id_cache)
            .and_then(exact_i32_const)
            .is_none()
        || chunk
            .jit_name_number(a.not_held_cache)
            .and_then(exact_i32_const)
            .is_none()
        || chunk
            .jit_name_number(b.not_held_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }

    let expected = |target: &crate::bytecode::InlineTarget| {
        target.pin.upgrade().map(|object| {
            let some: Option<crate::value::Gc> = Some(object);
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        })
    };
    let release_a = chunk.jit_inline_target(a.release_target);
    let release_b = chunk.jit_inline_target(b.release_target);
    let mark_a = chunk.jit_inline_target(a.mark_target);
    let mark_b = chunk.jit_inline_target(b.mark_target);
    if release_a.argc != 1
        || !release_a.check_this
        || release_b.argc != 1
        || !release_b.check_this
        || mark_a.argc != 0
        || !mark_a.check_this
        || mark_b.argc != 0
        || !mark_b.check_this
    {
        return None;
    }
    let release_expected = expected(release_a)?;
    let mark_expected = expected(mark_a)?;
    if expected(release_b)? != release_expected || expected(mark_b)? != mark_expected {
        return None;
    }
    Some(SchedulerIdleReleasePlan {
        count,
        v1,
        scheduler,
        release_method,
        release_expected,
        id_a_cache: chunk.jit_name_cache_ptr(a.id_cache),
        id_b_cache: chunk.jit_name_cache_ptr(b.id_cache),
        blocks,
        mark_method,
        mark_expected,
        state,
        not_held_cache: chunk.jit_name_cache_ptr(a.not_held_cache),
        target_priority,
        current,
        current_priority,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_shell(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerShellPlan> {
    use crate::bytecode::Op;
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !set_prop_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        return None;
    }
    let end = head.checked_add(28)?;
    let [
        Op::GetPropThis(current_name, current_cache0),
        Op::Const(null),
        Op::NotEq,
        Op::JumpIfFalse(null_pc),
        Op::GetPropThis(current_name1, current_cache1),
        Op::GetMethod(_, method_cache),
        Op::InlineGuard(inline_target, generic_call),
        Op::Pop,
        Op::StoreLocal(temp),
        Op::GetPropLocal(temp1, state_name, state_cache0),
        Op::LoadName(_, held_cache),
        Op::BitAnd,
        Op::Const(zero),
        Op::NotEq,
        Op::JumpIfTruePeek(or_join),
        Op::Pop,
        Op::GetPropLocal(temp2, state_name1, state_cache1),
        Op::LoadName(_, suspended_cache),
        Op::EqEq,
        Op::Jump(bool_join0),
        Op::Undef,
        Op::Jump(bool_join1),
        Op::CallWithThis(0, _),
        Op::JumpIfFalse(active_pc),
        Op::GetPropThis(current_name2, current_cache2),
        Op::GetProp(link_name, link_cache),
        Op::SetPropThisDrop(current_name3, current_store),
        Op::Jump(trampoline),
    ] = ops.get(head..end)?
    else {
        return None;
    };
    if current_name != current_name1
        || current_name != current_name2
        || current_name != current_name3
        || state_name != state_name1
        || temp != temp1
        || temp != temp2
        || *generic_call as usize != head + 22
        || *or_join as usize != head + 19
        || *bool_join0 as usize != head + 23
        || *bool_join1 as usize != head + 23
        || *active_pc as usize != end
        || ops.get(*trampoline as usize).and_then(|op| match op {
            Op::Jump(target) => Some(*target as usize),
            _ => None,
        }) != Some(head)
        || !chunk.jit_const_copyable(*null)
        || chunk.jit_const_bits(*null) != (2, 0)
        || chunk
            .jit_const_num(*zero)
            .and_then(exact_i32_const)
            != Some(0)
    {
        return None;
    }
    // The inlined method's temporary receiver is dead after the boolean join. Leaving that
    // compiler-only slot stale on the active side exit is therefore unobservable; any read would
    // require exact snapshot materialization and rejects this compact region.
    if ops[end..].iter().any(|op| {
        matches!(
            op,
            Op::LoadLocal(s)
                | Op::UpdateLocal(s, _)
                | Op::GetPropLocal(s, ..)
                | Op::SetPropLocalDrop(s, ..)
                | Op::GetElemLocal(s)
                | Op::SetElemLocal(s)
                | Op::SetElemLocalDrop(s)
                | Op::ToPropKeyLocal(s)
                | Op::IterStepL(s, _)
                | Op::IterStepL(_, s)
                | Op::IterCloseL(s)
                | Op::IterAbortL(s)
                if s == temp
        )
    }) {
        return None;
    }
    let temp_off = *temp as u32 * 16;
    if temp_off + 16 >= 4096 {
        return None;
    }
    let lp = cfg.loop_at_header(head)?;
    if !lp.latches.iter().any(|id| {
        let block = &cfg.blocks()[id.0 as usize];
        block.start <= *trampoline as usize && (*trampoline as usize) < block.end
    }) || crate::jit_ir::RegionIr::build_loop(chunk, cfg, head).is_err()
    {
        return None;
    }
    let current = chunk.jit_cache_preferred(*current_cache0)?;
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|state| {
            state.depth == 0
                && state.recv_shape == expected.recv_shape
                && state.slot == expected.slot
        })
    };
    if current.depth != 0
        || !same_own(*current_cache1, current)
        || !same_own(*current_cache2, current)
        || !same_own(*current_store, current)
    {
        return None;
    }
    let state = chunk.jit_cache_preferred(*state_cache0)?;
    let link = chunk.jit_cache_preferred(*link_cache)?;
    if state.depth != 0
        || link.depth != 0
        || state.recv_shape != link.recv_shape
        || !same_own(*state_cache1, state)
        || chunk.jit_name(*link_name).as_bytes().first().is_some_and(u8::is_ascii_digit)
    {
        return None;
    }
    let method = chunk.jit_cache_preferred(*method_cache)?;
    if method.depth != 1 || method.recv_shape != state.recv_shape {
        return None;
    }
    let target = chunk.jit_inline_target(*inline_target);
    if target.argc != 0 || !target.check_this {
        return None;
    }
    let method_expected = target.pin.upgrade().map(|o| {
        let some: Option<crate::value::Gc> = Some(o);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    })?;
    if chunk
        .jit_name_number(*held_cache)
        .and_then(exact_i32_const)
        .is_none()
        || chunk
            .jit_name_number(*suspended_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let mut active = plan_scheduler_active(chunk, ops, *active_pc as usize, current, state);
    if let Some(active_plan) = active.as_mut() {
        active_plan.null_dispatch = plan_scheduler_active_null_dispatch(
            chunk,
            ops,
            cfg,
            layout,
            fast,
            head,
            current,
            active_plan,
        );
    }
    Some(SchedulerShellPlan {
        head,
        active_pc: *active_pc as usize,
        null_pc: *null_pc as usize,
        temp_off,
        current,
        state,
        link,
        method,
        method_expected,
        held_cache: chunk.jit_name_cache_ptr(*held_cache),
        suspended_cache: chunk.jit_name_cache_ptr(*suspended_cache),
        active,
    })
}

/// Recognize the straight-line `TaskControlBlock.run` prologue immediately after the scheduler
/// shell.  The lowering stops before the polymorphic task dispatch, at a canonical empty-stack
/// boundary with the inlined TCB and packet locals fully materialized.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_active(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    start: usize,
    current: crate::bytecode::IcState,
    state: crate::bytecode::IcState,
) -> Option<SchedulerActivePlan> {
    use crate::bytecode::Op;
    macro_rules! reject {
        ($why:expr) => {{
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                eprintln!("[jit-region-plan] scheduler active: reject {}", $why);
            }
            return None;
        }};
    }
    if std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE").is_some() {
        reject!("disabled");
    }
    let end = start.checked_add(29)?;
    let [
        Op::GetPropThis(current_name0, current_cache0),
        Op::GetProp(_id_name, id_cache),
        Op::SetPropThisDrop(_current_id_name, current_id_store),
        Op::GetPropThis(current_name1, current_cache1),
        Op::GetMethod(_, run_cache),
        Op::InlineGuard(run_target, _),
        Op::Pop,
        Op::StoreLocal(tcb),
        Op::ResetSlots(reset_start, reset_count),
        Op::GetPropLocal(tcb1, state_name0, state_cache),
        Op::LoadName(_, suspended_runnable_cache),
        Op::EqEq,
        Op::JumpIfFalse(no_packet),
        Op::GetPropLocal(tcb2, queue_name0, queue_cache0),
        Op::StoreLocal(packet),
        Op::GetPropLocal(packet1, link_name, packet_link_cache),
        Op::SetPropLocalDrop(tcb3, queue_name1, queue_store),
        Op::GetPropLocal(tcb4, queue_name2, queue_cache1),
        Op::Const(null0),
        Op::EqEq,
        Op::JumpIfFalse(runnable_branch),
        Op::LoadName(_, running_cache),
        Op::SetPropLocalDrop(tcb5, state_name1, state_store0),
        Op::Jump(state_join),
        Op::LoadName(_, runnable_cache),
        Op::SetPropLocalDrop(tcb6, state_name2, state_store1),
        Op::Jump(exit),
        Op::Const(null1),
        Op::StoreLocal(packet2),
    ] = ops.get(start..end)?
    else {
        reject!("op shape");
    };
    if current_name0 != current_name1
        || state_name0 != state_name1
        || state_name0 != state_name2
        || queue_name0 != queue_name1
        || queue_name0 != queue_name2
        || tcb != tcb1
        || tcb != tcb2
        || tcb != tcb3
        || tcb != tcb4
        || tcb != tcb5
        || tcb != tcb6
        || tcb == packet
        || packet != packet1
        || packet != packet2
        || reset_start != packet
        || *reset_count != 1
        || *no_packet as usize != start + 27
        || *runnable_branch as usize != start + 24
        || *state_join as usize != start + 26
        || *exit as usize != end
        || !chunk.jit_const_copyable(*null0)
        || !chunk.jit_const_copyable(*null1)
        || chunk.jit_const_bits(*null0) != (2, 0)
        || chunk.jit_const_bits(*null1) != (2, 0)
        || chunk
            .jit_name(*link_name)
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
    {
        reject!("control shape");
    }
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    for (name, cache, expected) in [
        ("current read 0", *current_cache0, current),
        ("current read 1", *current_cache1, current),
        ("state read", *state_cache, state),
        ("state store 0", *state_store0, state),
        ("state store 1", *state_store1, state),
    ] {
        if !same_own(cache, expected) {
            reject!(name);
        }
    }
    let Some(id) = chunk.jit_cache_preferred(*id_cache) else {
        reject!("id cache");
    };
    let Some(current_id) = chunk.jit_cache_preferred(*current_id_store) else {
        reject!("currentId cache");
    };
    let Some(queue) = chunk.jit_cache_preferred(*queue_cache0) else {
        reject!("queue cache");
    };
    let Some(packet_link) = chunk.jit_cache_preferred(*packet_link_cache) else {
        reject!("packet link cache");
    };
    if id.depth != 0
        || id.recv_shape != state.recv_shape
        || current_id.depth != 0
        || current_id.recv_shape != current.recv_shape
        || queue.depth != 0
        || queue.recv_shape != state.recv_shape
        || !same_own(*queue_store, queue)
        || !same_own(*queue_cache1, queue)
        || packet_link.depth != 0
    {
        reject!("own-cache compatibility");
    }
    let Some(run_method) = chunk.jit_cache_preferred(*run_cache) else {
        reject!("run method cache");
    };
    let target = chunk.jit_inline_target(*run_target);
    if run_method.depth != 1
        || run_method.recv_shape != state.recv_shape
        || target.argc != 0
        || !target.check_this
    {
        reject!("run target");
    }
    let Some(run_expected) = target.pin.upgrade().map(|o| {
        let some: Option<crate::value::Gc> = Some(o);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    }) else {
        reject!("dead run target");
    };
    let Some(suspended_runnable) = chunk
        .jit_name_number(*suspended_runnable_cache)
        .and_then(exact_i32_const)
    else {
        reject!("suspended+runnable global");
    };
    let Some(running) = chunk
        .jit_name_number(*running_cache)
        .and_then(exact_i32_const)
    else {
        reject!("running global");
    };
    let Some(runnable) = chunk
        .jit_name_number(*runnable_cache)
        .and_then(exact_i32_const)
    else {
        reject!("runnable global");
    };
    let tcb_off = *tcb as u32 * 16;
    let packet_off = *packet as u32 * 16;
    if tcb_off + 16 >= 4096 || packet_off + 16 >= 4096 {
        reject!("slot offset");
    }
    Some(SchedulerActivePlan {
        exit_pc: end,
        tcb_off,
        packet_off,
        id,
        state,
        current_id,
        run_method,
        run_expected,
        queue,
        packet_link,
        suspended_runnable_cache: chunk.jit_name_cache_ptr(*suspended_runnable_cache),
        running_cache: chunk.jit_name_cache_ptr(*running_cache),
        runnable_cache: chunk.jit_name_cache_ptr(*runnable_cache),
        suspended_runnable,
        running,
        runnable,
        null_dispatch: None,
    })
}

/// Select the call-free Device/Handler tails that can consume SchedulerActive's exact Null packet
/// without first constructing wide frame locals. Requiring all three tails and a common scheduler
/// backedge makes the emitted continuation all-or-nothing and keeps its failure snapshot simple.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_active_null_dispatch(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
    loop_pc: usize,
    scheduler_current: crate::bytecode::IcState,
    active: &SchedulerActivePlan,
) -> Option<SchedulerActiveNullDispatchPlan> {
    if std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_NULL_STITCH").is_some()
        || cfg.stack_depth_at(active.exit_pc) != Some(0)
    {
        return None;
    }
    let device = plan_scheduler_device(chunk, ops, active.exit_pc, layout, fast)?;
    let mut handler = plan_scheduler_handler_suspend(chunk, ops, active.exit_pc, layout, fast)?;
    let suspend = device.suspend.as_ref()?;
    let queue = device.queue.as_ref()?;
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    if device.tcb_off != active.tcb_off
        || device.packet_off != active.packet_off
        || handler.tcb_off != active.tcb_off
        || handler.packet_off != active.packet_off
        || !same_ic(device.task, handler.task)
        || device.task.recv_shape != active.id.recv_shape
        || suspend.loop_pc != loop_pc
        || queue.loop_pc != loop_pc
        || handler.suspend.loop_pc != loop_pc
    {
        return None;
    }
    let handler_incoming_suspend =
        std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING_SUSPEND").is_none();
    let handler_incoming_work_delivery =
        std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_ACTIVE_WORK_DELIVERY").is_none();
    if handler_incoming_suspend || handler_incoming_work_delivery {
        let incoming = plan_scheduler_handler_incoming(
            chunk,
            ops,
            active.exit_pc,
            cfg,
            layout,
            fast,
            &handler,
        );
        let suspend_state = handler.suspend.state;
        let suspend_current = handler.suspend.current;
        handler.incoming = incoming.filter(|incoming| {
            same_ic(incoming.packet_link, active.packet_link)
                && same_ic(suspend_state, active.state)
                && same_ic(suspend_current, scheduler_current)
        });
    }
    let handler_incoming_suspend = handler_incoming_suspend && handler.incoming.is_some();
    let handler_incoming_work_delivery = handler_incoming_work_delivery
        && handler.incoming.as_ref().is_some_and(|incoming| {
            incoming.delivery.loop_pc == loop_pc
                && same_ic(incoming.delivery.current_id, active.current_id)
                && same_ic(incoming.delivery.current, scheduler_current)
                && same_ic(incoming.delivery.target_queue, active.queue)
                && same_ic(incoming.delivery.state, active.state)
        });
    handler.null_full = plan_scheduler_handler_null_full(
        chunk,
        ops,
        cfg,
        layout,
        fast,
        loop_pc,
        &handler,
    );
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region-plan] scheduler Active Handler null full={}, incoming_suspend={}, incoming_work_delivery={}",
            handler.null_full.is_some(),
            handler_incoming_suspend,
            handler_incoming_work_delivery
        );
    }
    let idle = plan_scheduler_active_idle(
        chunk,
        ops,
        layout,
        fast,
        loop_pc,
        scheduler_current,
        active,
        device.task,
    );
    let worker = plan_scheduler_active_worker(
        chunk,
        ops,
        active,
        device.task,
        *suspend,
        *queue,
    );
    Some(SchedulerActiveNullDispatchPlan {
        device,
        handler,
        handler_incoming_suspend,
        handler_incoming_work_delivery,
        idle,
        worker,
    })
}

/// Join HandlerTask's two already-warmed non-suspend plans to the Active null-packet prefix.
/// The child plans retain their complete live descriptor/method/value guard sets; this matcher
/// only proves that they describe the same Handler fields and return to the same scheduler loop.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_handler_null_full(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
    loop_pc: usize,
    wait: &SchedulerHandlerSuspendPlan,
) -> Option<SchedulerHandlerNullFullPlan> {
    if std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_NULL_FULL").is_some()
        || cfg.stack_depth_at(wait.completion_pc) != Some(0)
        || cfg.stack_depth_at(wait.delivery_pc) != Some(0)
    {
        return None;
    }
    let queue = plan_scheduler_handler_queue(chunk, ops, wait.completion_pc, layout, fast)?;
    let delivery =
        plan_scheduler_handler_deliver(chunk, ops, wait.delivery_pc, cfg, layout, fast)?;
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    if queue.handler_off != delivery.handler_off
        || queue.queue.loop_pc != loop_pc
        || delivery.loop_pc != loop_pc
        || !same_ic(wait.v1, queue.v1)
        || !same_ic(wait.v1, delivery.v1)
        || !same_ic(wait.v2, delivery.v2)
        || !same_ic(wait.packet_a1, delivery.work_a1)
        || !same_ic(queue.queue.scheduler, delivery.scheduler)
        || !scheduler_epoch_method_matches(
            queue.queue.queue_method,
            queue.queue.queue_expected,
            delivery.queue_method,
            delivery.queue_expected,
        )
    {
        return None;
    }
    Some(SchedulerHandlerNullFullPlan { delivery, queue })
}

/// Find IdleTask.run among the scheduler's remaining polymorphic call-cache ways, then recognize
/// its already-warmed whole-function release transaction in the exact child chunk recorded by
/// that way. The generated arm still guards TCB.task, IdleTask's shape, and the live run method.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_active_idle(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    layout: &crate::value::JitLayout,
    fast: u32,
    loop_pc: usize,
    scheduler_current: crate::bytecode::IcState,
    active: &SchedulerActivePlan,
    dispatch_task: crate::bytecode::IcState,
) -> Option<SchedulerActiveIdlePlan> {
    use crate::bytecode::Op;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_IDLE").is_some() {
        return None;
    }
    let [
        Op::GetPropLocal(tcb, _, task_cache),
        Op::GetMethod(_, run_cache),
        Op::LoadLocal(packet),
        Op::InlineGuard(_, device_guard),
    ] = ops.get(active.exit_pc..active.exit_pc.checked_add(4)?)?
    else {
        return None;
    };
    let Op::InlineGuard(_, generic_call) = ops.get(*device_guard as usize)? else {
        return None;
    };
    let Op::CallWithThis(1, call_cache) = ops.get(*generic_call as usize)? else {
        return None;
    };
    let after_call = match ops.get(*generic_call as usize + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let outer_current_store = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(_, cache)) => *cache,
        _ => return None,
    };
    let outer_loop = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if (*tcb as u32) * 16 != active.tcb_off
        || (*packet as u32) * 16 != active.packet_off
        || loop_pc >= active.exit_pc
    {
        return None;
    }
    let task = chunk.jit_cache_preferred(*task_cache)?;
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    if task.depth != 0
        || task.recv_shape != active.id.recv_shape
        || !same_ic(task, dispatch_task)
        || outer_loop != loop_pc
        || !chunk
            .jit_cache_preferred(outer_current_store)
            .is_some_and(|store| same_ic(store, scheduler_current))
    {
        return None;
    }

    let targets = chunk.jit_call_targets(*call_cache);
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region-plan] scheduler Active Idle: call_pc={}, ways={}",
            *generic_call,
            targets.len()
        );
    }
    let mut found = None;
    for (call_ic, callee) in targets {
        if call_ic.native != 0 || call_ic.chunk_raw.is_null() {
            continue;
        }
        let function = {
            let object = callee.borrow();
            let crate::value::Callable::User(user) = &object.call else {
                continue;
            };
            Rc::clone(&user.func)
        };
        let exact_child = [function.code2.get(), function.code.get()]
            .into_iter()
            .flatten()
            .filter_map(|candidate| candidate.as_ref())
            .find(|candidate| Rc::as_ptr(candidate) == call_ic.chunk_raw)
            .cloned();
        let Some(exact_child) = exact_child else {
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                eprintln!("[jit-region-plan] scheduler Active Idle: cached child mismatch");
            }
            continue;
        };
        // The call IC identifies the immutable JS function, but may still name its first-stage
        // chunk. Try both function-owned tiers: the second stage preserves the richer profile
        // needed by the whole-Idle recognizer, and the exact run-method guard below proves that
        // these cache pointers still belong to the live callee before generated code reads them.
        let mut release = None;
        for child in [function.code2.get(), function.code.get()]
            .into_iter()
            .flatten()
            .filter_map(|candidate| candidate.as_ref())
        {
            if release.is_some() {
                break;
            }
            let Ok(child_cfg) = crate::jit_ir::Cfg::build(child) else {
                continue;
            };
            let candidate = plan_scheduler_idle_release(
                child,
                child.jit_ops(),
                &child_cfg,
                layout,
                fast,
            );
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                eprintln!(
                    "[jit-region-plan] scheduler Active Idle: child_ops={}, exact={}, release={}",
                    child.jit_ops().len(),
                    Rc::ptr_eq(child, &exact_child),
                    candidate.is_some()
                );
            }
            release = candidate;
        }
        let Some(release) = release else {
            continue;
        };
        let Some(run_method) =
            chunk.jit_cache_for_shape(*run_cache, release.count.recv_shape)
        else {
            continue;
        };
        if run_method.depth != 1
            || run_method.recv_shape != release.count.recv_shape
            || !same_ic(release.current, scheduler_current)
        {
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                eprintln!(
                    "[jit-region-plan] scheduler Active Idle: compatibility reject run_depth={}, run_shape={}, idle_shape={}, current_match={}",
                    run_method.depth,
                    run_method.recv_shape,
                    release.count.recv_shape,
                    same_ic(release.current, scheduler_current)
                );
            }
            continue;
        }
        let run_expected = {
            let some: Option<crate::value::Gc> = Some(Rc::clone(&callee));
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        };
        if release.state.recv_shape != active.id.recv_shape
            || release.current_priority.recv_shape != active.id.recv_shape
        {
            continue;
        }
        let candidate = SchedulerActiveIdlePlan {
            task,
            run_method,
            run_expected,
            release,
        };
        if found.replace(candidate).is_some() {
            return None;
        }
    }
    found
}

/// Discover WorkerTask's exact-null arm from the scheduler's remaining polymorphic call ways.
/// The child body performs no mutation before `scheduler.suspendCurrent()`, so an exact run
/// identity plus live child property caches can bridge directly into the shared suspend tail.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_active_worker(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    active: &SchedulerActivePlan,
    dispatch_task: crate::bytecode::IcState,
    base_suspend: SchedulerDeviceSuspendPlan,
    base_queue: SchedulerDeviceQueuePlan,
) -> Option<SchedulerActiveWorkerPlan> {
    use crate::bytecode::Op;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_WORKER").is_some() {
        return None;
    }
    let [
        Op::GetPropLocal(tcb, _, task_cache),
        Op::GetMethod(_, run_cache),
        Op::LoadLocal(packet),
        Op::InlineGuard(_, device_guard),
    ] = ops.get(active.exit_pc..active.exit_pc.checked_add(4)?)?
    else {
        return None;
    };
    let Op::InlineGuard(_, generic_call) = ops.get(*device_guard as usize)? else {
        return None;
    };
    let Op::CallWithThis(1, call_cache) = ops.get(*generic_call as usize)? else {
        return None;
    };
    if (*tcb as u32) * 16 != active.tcb_off || (*packet as u32) * 16 != active.packet_off {
        return None;
    }
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    let task = chunk.jit_cache_preferred(*task_cache)?;
    if !same_ic(task, dispatch_task) {
        return None;
    }

    let mut found = None;
    for (call_ic, callee) in chunk.jit_call_targets(*call_cache) {
        if call_ic.native != 0 || call_ic.chunk_raw.is_null() {
            continue;
        }
        let function = {
            let object = callee.borrow();
            let crate::value::Callable::User(user) = &object.call else {
                continue;
            };
            Rc::clone(&user.func)
        };
        let candidates = [function.code2.get(), function.code.get()]
            .into_iter()
            .flatten()
            .filter_map(|candidate| candidate.as_ref());
        if !candidates
            .clone()
            .any(|candidate| Rc::as_ptr(candidate) == call_ic.chunk_raw)
        {
            continue;
        }
        for child in candidates {
            let child_ops = child.jit_ops();
            let Some(prefix) = child_ops.get(..9) else {
                continue;
            };
            let [
                Op::LoadLocal(arg),
                Op::Const(null),
                Op::EqEq,
                Op::JumpIfFalse(nonnull),
                Op::GetPropThis(_, scheduler_cache),
                Op::GetMethod(_, suspend_cache),
                Op::CallWithThis(0, _),
                Op::Return,
                Op::Jump(end),
            ] = prefix
            else {
                continue;
            };
            if child_ops.len() != 48
                || *arg != 0
                || *nonnull != 9
                || *end != 47
                || !matches!(child_ops[47], Op::ReturnUndef)
                || !child.jit_const_copyable(*null)
                || child.jit_const_bits(*null) != (2, 0)
            {
                continue;
            }
            let Some(scheduler) = child.jit_cache_preferred(*scheduler_cache) else {
                continue;
            };
            let Some(suspend_method) = child.jit_cache_preferred(*suspend_cache) else {
                continue;
            };
            let Some(run_method) =
                chunk.jit_cache_for_shape(*run_cache, scheduler.recv_shape)
            else {
                continue;
            };
            if scheduler.depth != 0
                || suspend_method.depth != 1
                || !same_ic(suspend_method, base_suspend.suspend_method)
                || run_method.depth != 1
                || run_method.recv_shape != scheduler.recv_shape
            {
                continue;
            }
            let run_expected = {
                let some: Option<crate::value::Gc> = Some(Rc::clone(&callee));
                unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
            };
            let mut suspend = base_suspend;
            suspend.scheduler = scheduler;
            let work = plan_scheduler_active_worker_work(
                child,
                active,
                scheduler,
                base_queue,
                &same_ic,
            );
            let candidate = SchedulerActiveWorkerPlan {
                task,
                run_method,
                run_expected,
                suspend,
                work,
            };
            if found.replace(candidate).is_some() {
                return None;
            }
        }
    }
    found
}

/// Recognize WorkerTask's exact packet arm in its compact first-stage chunk. The nested queue
/// transaction comes from the already-proven Device arm of the same outer scheduler; only the
/// Worker receiver cache and queue-method cache differ. Every cache consumed here is guarded
/// again by generated code after the exact Worker.run identity check.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_active_worker_work(
    child: &Chunk,
    active: &SchedulerActivePlan,
    scheduler: crate::bytecode::IcState,
    mut queue: SchedulerDeviceQueuePlan,
    same_ic: &impl Fn(crate::bytecode::IcState, crate::bytecode::IcState) -> bool,
) -> Option<SchedulerActiveWorkerWorkPlan> {
    use crate::bytecode::Op;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_WORKER_PACKET").is_some() {
        return None;
    }
    let ops = child.jit_ops();
    let [
        Op::GetPropThis(v1_name0, v1_cache0),
        Op::LoadName(id_a_name0, id_a_cache0),
        Op::EqEq,
        Op::JumpIfFalse(v1_else),
        Op::LoadName(_id_b_name, id_b_cache),
        Op::SetPropThisDrop(v1_name1, v1_store0),
        Op::Jump(v1_join),
        Op::LoadName(id_a_name1, id_a_cache1),
        Op::SetPropThisDrop(v1_name2, v1_store1),
        Op::GetPropThis(v1_name3, v1_cache1),
        Op::SetPropLocalDrop(packet0, _packet_id_name, packet_id_store),
        Op::Const(zero0),
        Op::SetPropLocalDrop(packet1, _packet_a1_name, packet_a1_store),
    ] = ops.get(9..22)?
    else {
        return None;
    };
    let [
        Op::Const(zero1),
        Op::StoreLocal(index0),
        Op::LoadLocal(index1),
        Op::LoadName(_data_size_name, data_size_cache),
        Op::Lt,
        Op::JumpIfFalse(loop_exit),
        Op::LoadThis,
        Op::UpdateProp(v2_name0, v2_update, crate::bytecode::UpdKind::IncDiscard),
        Op::GetPropThis(v2_name1, v2_cache0),
        Op::Const(threshold),
        Op::Gt,
        Op::JumpIfFalse(no_reset),
        Op::Const(reset),
        Op::SetPropThisDrop(v2_name2, v2_store),
        Op::GetPropLocal(packet2, _packet_a2_name, packet_a2_cache),
        Op::LoadLocal(index2),
        Op::GetPropThis(v2_name3, v2_cache1),
        Op::SetElemDrop,
        Op::UpdateLocal(index3, crate::bytecode::UpdKind::IncDiscard),
        Op::Jump(loop_head),
    ] = ops.get(22..42)?
    else {
        return None;
    };
    let [
        Op::GetPropThis(_scheduler_name, scheduler_cache),
        Op::GetMethod(_queue_name, queue_method_cache),
        Op::LoadLocal(packet3),
        Op::CallWithThis(1, queue_call_cache),
        Op::Return,
        Op::ReturnUndef,
    ] = ops.get(42..48)?
    else {
        return None;
    };
    if ops.len() != 48
        || *v1_else != 16
        || *v1_join != 18
        || *loop_exit != 42
        || *no_reset != 36
        || *loop_head != 24
        || v1_name0 != v1_name1
        || v1_name0 != v1_name2
        || v1_name0 != v1_name3
        || v2_name0 != v2_name1
        || v2_name0 != v2_name2
        || v2_name0 != v2_name3
        || id_a_name0 != id_a_name1
        || packet0 != packet1
        || packet0 != packet2
        || packet0 != packet3
        || *packet0 != 0
        || index0 != index1
        || index0 != index2
        || index0 != index3
        || *index0 != 1
        || !child.jit_const_copyable(*zero0)
        || !child.jit_const_copyable(*zero1)
        || child.jit_const_num(*zero0).and_then(exact_i32_const) != Some(0)
        || child.jit_const_num(*zero1).and_then(exact_i32_const) != Some(0)
    {
        return None;
    }
    let threshold = child.jit_const_num(*threshold).and_then(exact_i32_const)?;
    let reset = child.jit_const_num(*reset).and_then(exact_i32_const)?;
    if threshold < 1 || reset < 0 || reset > threshold {
        return None;
    }
    let v1 = child.jit_cache_preferred(*v1_cache0)?;
    let v2 = child.jit_cache_preferred(*v2_update)?;
    let packet_id = child.jit_cache_preferred(*packet_id_store)?;
    let packet_a1 = child.jit_cache_preferred(*packet_a1_store)?;
    let packet_a2 = child.jit_cache_preferred(*packet_a2_cache)?;
    let worker_scheduler = child.jit_cache_preferred(*scheduler_cache)?;
    let queue_method = child.jit_cache_preferred(*queue_method_cache)?;
    let own = |cache: u32, expected: crate::bytecode::IcState| {
        child
            .jit_cache_preferred(cache)
            .is_some_and(|actual| actual.depth == 0 && same_ic(actual, expected))
    };
    if v1.depth != 0
        || v2.depth != 0
        || v1.recv_shape != scheduler.recv_shape
        || v2.recv_shape != scheduler.recv_shape
        || v1.slot == v2.slot
        || !own(*v1_store0, v1)
        || !own(*v1_store1, v1)
        || !own(*v1_cache1, v1)
        || !own(*v2_cache0, v2)
        || !own(*v2_store, v2)
        || !own(*v2_cache1, v2)
        || worker_scheduler.depth != 0
        || !same_ic(worker_scheduler, scheduler)
        || queue_method.depth != 1
        || !same_ic(queue_method, queue.queue_method)
        || packet_id.depth != 0
        || packet_a1.depth != 0
        || packet_a2.depth != 0
        || packet_id.recv_shape != packet_a1.recv_shape
        || packet_id.recv_shape != packet_a2.recv_shape
        || !same_ic(packet_id, queue.packet_id)
        || !same_ic(queue.packet_link, active.packet_link)
        || !same_ic(queue.current_id, active.current_id)
        || !same_ic(queue.state, active.state)
    {
        return None;
    }
    let (call_ic, callee) = child.jit_call_target(*queue_call_cache)?;
    let queue_expected = {
        let some: Option<crate::value::Gc> = Some(callee);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    if call_ic.native != 0 || queue_expected != queue.queue_expected {
        return None;
    }
    if child
        .jit_name_number(*id_a_cache0)
        .and_then(exact_i32_const)
        .is_none()
        || child
            .jit_name_number(*id_b_cache)
            .and_then(exact_i32_const)
            .is_none()
        || child
            .jit_name_number(*id_a_cache1)
            .and_then(exact_i32_const)
            .is_none()
        || child
            .jit_name_number(*data_size_cache)
            .and_then(exact_i32_const)
            != Some(4)
    {
        return None;
    }
    queue.scheduler = worker_scheduler;
    queue.queue_method = queue_method;
    Some(SchedulerActiveWorkerWorkPlan {
        v1,
        v2,
        packet_a1,
        packet_a2,
        id_a_cache: child.jit_name_cache_ptr(*id_a_cache0),
        id_a_else_cache: child.jit_name_cache_ptr(*id_a_cache1),
        id_b_cache: child.jit_name_cache_ptr(*id_b_cache),
        data_size_cache: child.jit_name_cache_ptr(*data_size_cache),
        threshold,
        reset,
        queue,
    })
}

/// Recognize HandlerTask's exact-null/no-work arm at the scheduler's virtual task dispatch. Half
/// of Handler invocations take this path in Richards. All guards precede the shared suspend
/// transaction, so any mutation, accessor, exotic value, or alternate inline layout replays the
/// original task call with an untouched frame.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_handler_suspend(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerHandlerSuspendPlan> {
    use crate::bytecode::Op;
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !get_method_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        return None;
    }
    let [
        Op::GetPropLocal(tcb, _, task_cache),
        Op::GetMethod(_, run_cache),
        Op::LoadLocal(packet),
        Op::InlineGuard(handler_target, device_guard),
    ] = ops.get(head..head.checked_add(4)?)?
    else {
        return None;
    };
    let body = head + 4;
    let [
        Op::StoreLocal(handler_packet),
        Op::Pop,
        Op::StoreLocal(handler_task),
        Op::ResetSlots(reset_start, reset_count),
        Op::LoadLocal(handler_packet0),
        Op::Const(null_packet),
        Op::NotEq,
        Op::JumpIfFalse(no_packet),
    ] = ops.get(body..body.checked_add(8)?)?
    else {
        return None;
    };
    let no_packet = *no_packet as usize;
    let [
        Op::GetPropLocal(handler_task0, v1_name0, v1_cache),
        Op::Const(null_v1),
        Op::NotEq,
        Op::JumpIfFalse(suspend_start),
    ] = ops.get(no_packet..no_packet.checked_add(4)?)?
    else {
        return None;
    };
    let suspend_start = *suspend_start as usize;
    let [
        Op::GetPropLocal(handler_task2, v1_name1, v1_cache1),
        Op::GetProp(_, packet_a1_cache),
        Op::StoreLocal(count),
        Op::LoadLocal(count0),
        Op::LoadName(_, data_size_cache),
        Op::Lt,
        Op::JumpIfFalse(completion_pc),
        Op::GetPropLocal(handler_task3, _, v2_cache),
        Op::Const(null_v2),
        Op::NotEq,
        Op::JumpIfFalse(suspend_trampoline),
    ] = ops.get(no_packet + 4..no_packet.checked_add(15)?)?
    else {
        return None;
    };
    let [
        Op::GetPropLocal(handler_task1, scheduler_name, scheduler_cache),
        Op::GetMethod(suspend_name, _),
        ..,
    ] = ops.get(suspend_start..suspend_start.checked_add(24)?)?
    else {
        return None;
    };
    let expected_join = match ops.get(suspend_start + 23) {
        Some(Op::Jump(join)) => *join as usize,
        _ => return None,
    };
    if *handler_packet != *handler_packet0
        || *handler_task != *handler_task0
        || *handler_task != *handler_task1
        || *handler_task != *handler_task2
        || *handler_task != *handler_task3
        || *handler_packet == *handler_task
        || v1_name0 != v1_name1
        || count != count0
        || *reset_count != 2
        || *reset_start == *handler_packet
        || *reset_start == *handler_task
        || *device_guard as usize <= suspend_start + 23
        || !matches!(ops.get(*suspend_trampoline as usize), Some(Op::Jump(t)) if *t as usize == suspend_start)
        || ![null_packet, null_v1, null_v2]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }
    let target = chunk.jit_inline_target(*handler_target);
    if target.argc != 1 || !target.check_this {
        return None;
    }
    let run_expected = target.pin.upgrade().map(|o| {
        let some: Option<crate::value::Gc> = Some(o);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    })?;
    let task = chunk.jit_cache_preferred(*task_cache)?;
    let v1 = chunk.jit_cache_preferred(*v1_cache)?;
    let v2 = chunk.jit_cache_preferred(*v2_cache)?;
    let packet_a1 = chunk.jit_cache_preferred(*packet_a1_cache)?;
    let run_method = chunk.jit_cache_for_shape(*run_cache, v1.recv_shape)?;
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    if task.depth != 0
        || v1.depth != 0
        || !same_own(*v1_cache1, v1)
        || v2.depth != 0
        || v2.recv_shape != v1.recv_shape
        || v2.slot == v1.slot
        || packet_a1.depth != 0
        || packet_a1.recv_shape == v1.recv_shape
        || packet_a1.recv_shape == task.recv_shape
        || task.recv_shape == v1.recv_shape
        || run_method.depth != 1
        || run_method.recv_shape != v1.recv_shape
        || chunk
            .jit_name_number(*data_size_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let tcb_off = *tcb as u32 * 16;
    let packet_off = *packet as u32 * 16;
    if tcb == packet || tcb_off + 16 >= 4096 || packet_off + 16 >= 4096 {
        return None;
    }
    let suspend = plan_scheduler_device_suspend(
        chunk,
        ops,
        *device_guard as usize,
        *scheduler_name,
        *scheduler_cache,
        *suspend_name,
        task.recv_shape,
        v1.recv_shape,
        expected_join,
    )?;
    Some(SchedulerHandlerSuspendPlan {
        tcb_off,
        packet_off,
        completion_pc: *completion_pc as usize,
        delivery_pc: no_packet + 15,
        task,
        run_method,
        run_expected,
        v1,
        v2,
        packet_a1,
        data_size_cache: chunk.jit_name_cache_ptr(*data_size_cache),
        suspend,
        incoming: None,
        null_full: None,
    })
}

/// Recognize HandlerTask's incoming non-work packet prefix and bridge directly into the existing
/// v2-delivery transaction. Canonical Richards takes this DEVICE arm 781 times per run. The
/// prefix's exact Packet.addTo body only clears an already-Null link and returns the packet, so a
/// guarded owner can be published to Handler.v2 before the compiler locals are materialized.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_handler_incoming(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
    wait: &SchedulerHandlerSuspendPlan,
) -> Option<SchedulerHandlerIncomingPlan> {
    use crate::bytecode::Op;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING").is_some()
        || !set_prop_inlinable(layout)
        || cfg.stack_depth_at(head) != Some(0)
    {
        return None;
    }

    let body = head.checked_add(4)?;
    let [
        Op::StoreLocal(handler_packet),
        Op::Pop,
        Op::StoreLocal(handler),
        Op::ResetSlots(count, reset_count),
        Op::LoadLocal(handler_packet0),
        Op::Const(null_packet),
        Op::NotEq,
        Op::JumpIfFalse(no_packet),
        Op::GetPropLocal(handler_packet1, _, kind_cache),
        Op::LoadName(_, kind_work_cache),
        Op::EqEq,
        Op::JumpIfFalse(device_start),
    ] = ops.get(body..body.checked_add(12)?)?
    else {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject prefix header");
        }
        return None;
    };
    let device_start = *device_start as usize;
    let no_packet = *no_packet as usize;
    let work_start = body.checked_add(12)?;
    let delivery_pc = no_packet.checked_add(15)?;
    if *handler_packet != *handler_packet0
        || *handler_packet != *handler_packet1
        || *reset_count != 2
        || work_start.checked_add(36)? != device_start
        || device_start.checked_add(35)? != no_packet
        || !chunk.jit_const_copyable(*null_packet)
        || chunk.jit_const_bits(*null_packet) != (2, 0)
        || cfg.stack_depth_at(delivery_pc) != Some(0)
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject prefix header invariants");
        }
        return None;
    }
    let saved = count.checked_add(1)?;
    if handler_packet.checked_add(1) != Some(*handler)
        || handler.checked_add(1) != Some(*count)
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject Handler local group");
        }
        return None;
    }

    // WORK contains the same inlined Packet.addTo body as DEVICE, followed by the explicit jump
    // over the else arm. The two sites have distinct IC indices, so prove their warmed method,
    // packet-link, scan-link, and Handler-field facts independently before sharing an Active
    // transaction.
    let [
        Op::LoadLocal(work_packet),
        Op::GetMethod(_, work_add_method_cache),
        Op::GetPropLocal(work_handler0, work_v1_name0, work_v1_cache),
        Op::InlineGuard(work_add_target, work_generic_call),
        Op::StoreLocal(work_add_queue),
        Op::Pop,
        Op::StoreLocal(work_add_this),
        Op::ResetSlots(work_add_next, work_add_reset_count),
        Op::Const(work_null_link0),
        Op::SetPropLocalDrop(work_add_this0, work_link_name0, work_packet_link_store),
        Op::LoadLocal(work_add_queue0),
        Op::Const(work_null_queue),
        Op::EqEq,
        Op::JumpIfFalse(work_nonempty_queue),
        Op::LoadLocal(work_add_this1),
        Op::Jump(work_add_join0),
        Op::LoadLocal(work_add_queue1),
        Op::StoreLocal(work_next),
        Op::GetPropLocal(work_next0, work_link_name1, work_queued_link_cache),
        Op::Dup,
        Op::StoreLocal(work_peek),
        Op::Const(work_null_peek),
        Op::NotEq,
        Op::JumpIfFalse(work_scan_done),
        Op::LoadLocal(work_peek0),
        Op::StoreLocal(work_next1),
        Op::Jump(work_scan_head),
        Op::LoadLocal(work_add_this2),
        Op::SetPropLocalDrop(work_next2, work_link_name2, work_queued_link_store),
        Op::LoadLocal(work_add_queue2),
        Op::Jump(work_add_join1),
        Op::Undef,
        Op::Jump(work_add_join2),
        Op::CallWithThis(1, _),
        Op::SetPropLocalDrop(work_handler1, work_v1_name1, work_v1_store),
        Op::Jump(work_exit),
    ] = ops.get(work_start..device_start)?
    else {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject WORK Packet.addTo body");
        }
        return None;
    };
    if handler_packet != work_packet
        || handler != work_handler0
        || handler != work_handler1
        || work_v1_name0 != work_v1_name1
        || work_add_queue != work_add_queue0
        || work_add_queue != work_add_queue1
        || work_add_queue != work_add_queue2
        || work_add_this != work_add_this0
        || work_add_this != work_add_this1
        || work_add_this != work_add_this2
        || work_next != work_next0
        || work_next != work_next1
        || work_next != work_next2
        || work_peek != work_peek0
        || work_add_next != work_peek
        || work_add_next.checked_add(1) != Some(*work_next)
        || work_add_queue.checked_add(1) != Some(*work_add_this)
        || work_add_this.checked_add(1) != Some(*work_add_next)
        || work_link_name0 != work_link_name1
        || work_link_name0 != work_link_name2
        || *work_add_reset_count != 2
        || *work_generic_call as usize != work_start + 33
        || *work_nonempty_queue as usize != work_start + 16
        || *work_add_join0 as usize != work_start + 34
        || *work_scan_done as usize != work_start + 27
        || *work_scan_head as usize != work_start + 18
        || *work_add_join1 as usize != work_start + 34
        || *work_add_join2 as usize != work_start + 34
        || *work_exit as usize != no_packet
        || ![work_null_link0, work_null_queue, work_null_peek]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject WORK addTo invariants");
        }
        return None;
    }

    let [
        Op::LoadLocal(handler_packet2),
        Op::GetMethod(_, add_method_cache),
        Op::GetPropLocal(handler0, v2_name0, v2_cache),
        Op::InlineGuard(add_target, generic_call),
        Op::StoreLocal(add_queue),
        Op::Pop,
        Op::StoreLocal(add_this),
        Op::ResetSlots(add_next, add_reset_count),
        Op::Const(null_link0),
        Op::SetPropLocalDrop(add_this0, link_name0, packet_link_store),
        Op::LoadLocal(add_queue0),
        Op::Const(null_queue),
        Op::EqEq,
        Op::JumpIfFalse(nonempty_queue),
        Op::LoadLocal(add_this1),
        Op::Jump(add_join0),
        Op::LoadLocal(add_queue1),
        Op::StoreLocal(next),
        Op::GetPropLocal(next0, link_name1, queued_link_cache),
        Op::Dup,
        Op::StoreLocal(peek),
        Op::Const(null_peek),
        Op::NotEq,
        Op::JumpIfFalse(scan_done),
        Op::LoadLocal(peek0),
        Op::StoreLocal(next1),
        Op::Jump(scan_head),
        Op::LoadLocal(add_this2),
        Op::SetPropLocalDrop(next2, link_name2, queued_link_store),
        Op::LoadLocal(add_queue2),
        Op::Jump(add_join1),
        Op::Undef,
        Op::Jump(add_join2),
        Op::CallWithThis(1, _),
        Op::SetPropLocalDrop(handler1, v2_name1, v2_store),
    ] = ops.get(device_start..no_packet)?
    else {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject Packet.addTo body");
        }
        return None;
    };
    if *handler_packet != *handler_packet2
        || handler != handler0
        || handler != handler1
        || v2_name0 != v2_name1
        || add_queue != add_queue0
        || add_queue != add_queue1
        || add_queue != add_queue2
        || add_this != add_this0
        || add_this != add_this1
        || add_this != add_this2
        || next != next0
        || next != next1
        || next != next2
        || peek != peek0
        || add_next != peek
        || add_next.checked_add(1) != Some(*next)
        || add_queue.checked_add(1) != Some(*add_this)
        || add_this.checked_add(1) != Some(*add_next)
        || link_name0 != link_name1
        || link_name0 != link_name2
        || *add_reset_count != 2
        || *generic_call as usize != device_start + 33
        || *nonempty_queue as usize != device_start + 16
        || *add_join0 as usize != device_start + 34
        || *scan_done as usize != device_start + 27
        || *scan_head as usize != device_start + 18
        || *add_join1 as usize != device_start + 34
        || *add_join2 as usize != device_start + 34
        || ![null_link0, null_queue, null_peek]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject Packet.addTo invariants");
        }
        return None;
    }

    let Some(delivery) = plan_scheduler_handler_deliver(chunk, ops, delivery_pc, cfg, layout, fast)
    else {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject delivery plan");
        }
        return None;
    };
    let handler_packet_off = *handler_packet as u32 * 16;
    let handler_off = *handler as u32 * 16;
    let count_off = *count as u32 * 16;
    let saved_off = saved as u32 * 16;
    let add_queue_off = *add_queue as u32 * 16;
    let add_this_off = *add_this as u32 * 16;
    let add_next_off = *add_next as u32 * 16;
    let add_last_off = add_next.checked_add(1)? as u32 * 16;
    let offsets = [
        wait.tcb_off,
        wait.packet_off,
        handler_packet_off,
        handler_off,
        count_off,
        saved_off,
        add_queue_off,
        add_this_off,
        add_next_off,
        add_last_off,
    ];
    if offsets
        .iter()
        .enumerate()
        .any(|(i, off)| *off + 16 >= 4096 || offsets[..i].contains(off))
        || delivery.handler_off != handler_off
        || delivery.count_off != count_off
        || delivery.saved_off != saved_off
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject local layout");
        }
        return None;
    }

    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let same_state = |actual: crate::bytecode::IcState,
                      expected: crate::bytecode::IcState| {
        actual.depth == expected.depth
            && actual.recv_shape == expected.recv_shape
            && actual.holder_shape == expected.holder_shape
            && actual.slot == expected.slot
    };
    let kind = chunk.jit_cache_preferred(*kind_cache)?;
    let add_method = chunk.jit_cache_preferred(*add_method_cache)?;
    let incoming_v2 = chunk.jit_cache_preferred(*v2_cache)?;
    let packet_link = chunk.jit_cache_preferred(*packet_link_store)?;
    let queued_link = chunk.jit_cache_preferred(*queued_link_cache)?;
    let work_add_method = chunk.jit_cache_preferred(*work_add_method_cache)?;
    let incoming_v1 = chunk.jit_cache_preferred(*work_v1_cache)?;
    let work_packet_link = chunk.jit_cache_preferred(*work_packet_link_store)?;
    let work_queued_link = chunk.jit_cache_preferred(*work_queued_link_cache)?;
    let target = chunk.jit_inline_target(*add_target);
    let work_target = chunk.jit_inline_target(*work_add_target);
    if target.argc != 1
        || !target.check_this
        || work_target.argc != 1
        || !work_target.check_this
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject inline target ABI");
        }
        return None;
    }
    let add_expected = target.pin.upgrade().map(|obj| {
        let some: Option<crate::value::Gc> = Some(obj);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    })?;
    let work_add_expected = work_target.pin.upgrade().map(|obj| {
        let some: Option<crate::value::Gc> = Some(obj);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    })?;
    if kind.depth != 0
        || kind.recv_shape != delivery.packet_id.recv_shape
        || add_method.depth != 1
        || add_method.recv_shape != kind.recv_shape
        || !same_state(work_add_method, add_method)
        || work_add_expected != add_expected
        || add_expected != delivery.add_expected
        || !same_state(add_method, delivery.add_method)
        || !same_state(incoming_v2, wait.v2)
        || !same_state(incoming_v2, delivery.v2)
        || !same_own(*v2_store, incoming_v2)
        || !same_state(incoming_v1, wait.v1)
        || !same_state(incoming_v1, delivery.v1)
        || !same_own(*work_v1_store, incoming_v1)
        || !same_state(packet_link, delivery.packet_link)
        || !same_state(work_packet_link, packet_link)
        || !same_state(queued_link, delivery.queued_link)
        || !same_state(queued_link, packet_link)
        || !same_state(work_queued_link, queued_link)
        || !same_own(*queued_link_store, queued_link)
        || !same_own(*work_queued_link_store, work_queued_link)
        || !same_state(wait.v1, delivery.v1)
        || !same_state(wait.packet_a1, delivery.work_a1)
        || chunk
            .jit_name_number(*kind_work_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
            eprintln!("[jit-region-plan] Handler incoming: reject warmed cache compatibility");
        }
        return None;
    }

    Some(SchedulerHandlerIncomingPlan {
        kind,
        kind_work_cache: chunk.jit_name_cache_ptr(*kind_work_cache),
        add_method,
        add_expected,
        packet_link,
        delivery,
    })
}

/// Recognize HandlerTask's `v = v1; v1 = v1.link; return scheduler.queue(v)` arm at its
/// empty-stack branch target. The queue/check/mark transaction is flattened only for an empty,
/// non-preempting target queue; every descriptor, shape, method, global, and value-class guard
/// runs before either ownership move is published.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_handler_queue(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerHandlerQueuePlan> {
    use crate::bytecode::{Op, UpdKind};
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_QUEUE").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !set_prop_inlinable(layout)
        || !get_method_inlinable(layout)
        || !elem_inlinable(layout)
        || !packed_elem_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        return None;
    }

    let end = head.checked_add(70)?;
    let [
        Op::GetPropLocal(handler, v1_name0, v1_cache0),
        Op::StoreLocal(saved),
        Op::GetPropLocal(handler1, v1_name1, v1_cache1),
        Op::GetProp(link_name0, packet_link_cache),
        Op::SetPropLocalDrop(handler2, v1_name2, v1_store),
        Op::GetPropLocal(handler3, _, scheduler_cache),
        Op::GetMethod(_, queue_method_cache),
        Op::LoadLocal(saved0),
        Op::InlineGuard(queue_target, queue_generic),
        Op::StoreLocal(packet),
        Op::Pop,
        Op::StoreLocal(scheduler),
        Op::ResetSlots(target, reset_count),
        Op::GetPropLocal(scheduler0, _, blocks_cache),
        Op::GetPropLocal(packet0, id_name0, packet_id_cache),
        Op::GetElem,
        Op::StoreLocal(target0),
        Op::LoadLocal(target1),
        Op::Const(null_target),
        Op::EqEq,
        Op::JumpIfFalse(nonnull_target),
        Op::LoadLocal(target2),
        Op::Jump(null_return),
        Op::LoadLocal(scheduler1),
        Op::UpdateProp(_, queue_count_cache, UpdKind::IncDiscard),
        Op::Const(null_link),
        Op::SetPropLocalDrop(packet1, link_name1, packet_link_store),
        Op::GetPropLocal(scheduler2, _, current_id_cache),
        Op::SetPropLocalDrop(packet2, id_name1, packet_id_store),
        Op::LoadLocal(target3),
        Op::GetMethod(_, check_method_cache),
        Op::GetPropLocal(scheduler3, current_name0, current_cache),
        Op::LoadLocal(packet3),
        Op::InlineGuard(check_target, check_generic),
        Op::StoreLocal(check_packet),
        Op::StoreLocal(check_task),
        Op::Pop,
        Op::StoreLocal(check_this),
        Op::GetPropLocal(check_this0, queue_name0, target_queue_cache),
        Op::Const(null_queue),
        Op::EqEq,
        Op::JumpIfFalse(nonempty_queue),
        Op::LoadLocal(check_packet0),
        Op::SetPropLocalDrop(check_this1, queue_name1, target_queue_store),
        Op::LoadLocal(check_this2),
        Op::GetMethod(_, mark_method_cache),
        Op::CallWithThis(0, mark_call),
        Op::Pop,
        Op::GetPropLocal(check_this3, priority_name0, target_priority_cache),
        Op::GetPropLocal(check_task0, priority_name1, current_priority_cache),
        Op::Gt,
        Op::JumpIfFalse(no_preempt),
        Op::LoadLocal(check_this4),
        Op::Jump(return_join0),
        Op::Jump(return_current),
        Op::LoadLocal(check_packet1),
        Op::GetMethod(_, _),
        Op::GetPropLocal(check_this5, queue_name2, _),
        Op::CallWithThis(1, _),
        Op::SetPropLocalDrop(check_this6, queue_name3, _),
        Op::LoadLocal(check_task1),
        Op::Jump(return_join1),
        Op::Undef,
        Op::Jump(return_join2),
        Op::CallWithThis(2, _),
        Op::Jump(queue_join0),
        Op::Undef,
        Op::Jump(queue_join1),
        Op::CallWithThis(1, _),
        Op::Jump(outer_join),
    ] = ops.get(head..end)?
    else {
        return None;
    };

    let after_call = match ops.get(*outer_join as usize) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let (current_name1, outer_current_store) = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(name, cache)) => (*name, *cache),
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if handler != handler1
        || handler != handler2
        || handler != handler3
        || saved != saved0
        || v1_name0 != v1_name1
        || v1_name0 != v1_name2
        || link_name0 != link_name1
        || id_name0 != id_name1
        || current_name0 != &current_name1
        || queue_name0 != queue_name1
        || queue_name0 != queue_name2
        || queue_name0 != queue_name3
        || priority_name0 != priority_name1
        || scheduler != scheduler0
        || scheduler != scheduler1
        || scheduler != scheduler2
        || scheduler != scheduler3
        || packet != packet0
        || packet != packet1
        || packet != packet2
        || packet != packet3
        || target != target0
        || target != target1
        || target != target2
        || target != target3
        || check_packet != check_packet0
        || check_packet != check_packet1
        || check_task != check_task0
        || check_task != check_task1
        || check_this != check_this0
        || check_this != check_this1
        || check_this != check_this2
        || check_this != check_this3
        || check_this != check_this4
        || check_this != check_this5
        || check_this != check_this6
        || *reset_count != 1
        || *queue_generic as usize != head + 68
        || *nonnull_target as usize != head + 23
        || *null_return as usize != head + 69
        || *check_generic as usize != head + 64
        || *nonempty_queue as usize != head + 55
        || *no_preempt as usize != head + 54
        || *return_join0 as usize != head + 65
        || *return_current as usize != head + 60
        || *return_join1 as usize != head + 65
        || *return_join2 as usize != head + 65
        || *queue_join0 as usize != head + 69
        || *queue_join1 as usize != head + 69
        || after_call != *outer_join as usize + 4
        || ![null_target, null_link, null_queue]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }
    let locals = [
        *handler,
        *saved,
        *packet,
        *scheduler,
        *target,
        *check_packet,
        *check_task,
        *check_this,
    ];
    if locals
        .iter()
        .enumerate()
        .any(|(i, slot)| locals[..i].contains(slot))
    {
        return None;
    }
    let handler_off = *handler as u32 * 16;
    if handler_off + 16 >= 4096 {
        return None;
    }

    let v1 = chunk.jit_cache_preferred(*v1_cache0)?;
    let handler_scheduler = chunk.jit_cache_preferred(*scheduler_cache)?;
    let queue_method = chunk.jit_cache_preferred(*queue_method_cache)?;
    let blocks = chunk.jit_cache_preferred(*blocks_cache)?;
    let packet_id = chunk.jit_cache_preferred(*packet_id_cache)?;
    let packet_link = chunk.jit_cache_preferred(*packet_link_cache)?;
    let queue_count = chunk.jit_cache_preferred(*queue_count_cache)?;
    let current_id = chunk.jit_cache_preferred(*current_id_cache)?;
    let check_method = chunk.jit_cache_preferred(*check_method_cache)?;
    let current = chunk.jit_cache_preferred(*current_cache)?;
    let target_queue = chunk.jit_cache_preferred(*target_queue_cache)?;
    let mark_method = chunk.jit_cache_preferred(*mark_method_cache)?;
    let target_priority = chunk.jit_cache_preferred(*target_priority_cache)?;
    let current_priority = chunk.jit_cache_preferred(*current_priority_cache)?;
    let same_own = |owner: &Chunk, cache: u32, expected: crate::bytecode::IcState| {
        owner.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let distinct_slots = |states: &[crate::bytecode::IcState]| {
        states.iter().enumerate().all(|(i, state)| {
            states[..i]
                .iter()
                .all(|earlier| earlier.slot != state.slot)
        })
    };

    let queue_target = chunk.jit_inline_target(*queue_target);
    let check_target = chunk.jit_inline_target(*check_target);
    if queue_target.argc != 1
        || !queue_target.check_this
        || check_target.argc != 2
        || !check_target.check_this
    {
        return None;
    }
    let expected = |target: &crate::bytecode::InlineTarget| {
        target.pin.upgrade().map(|o| {
            let some: Option<crate::value::Gc> = Some(o);
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        })
    };

    // This mark call sits beyond the inlining budget. A freshly-created second-stage scheduler
    // chunk has not executed that CallWithThis yet, so first try its cache and then consult the
    // already-warmed exact checkPriorityAdd function whose body supplied this inline expansion.
    // In either case, parse the exact five-op child and use the CallIc's actual chunk generation.
    let parse_mark = |owner: &Chunk,
                      call: u32|
     -> Option<(crate::value::Gc, crate::bytecode::IcState, usize)> {
        let (mark_ic, mark_obj) = owner.jit_call_target(call)?;
        if mark_ic.native != 0 || mark_ic.chunk_raw.is_null() {
            return None;
        }
        let mark_func = match &mark_obj.borrow().call {
            crate::value::Callable::User(user) => Rc::clone(&user.func),
            _ => return None,
        };
        if mark_func.is_arrow
            || mark_func.is_generator
            || mark_func.is_async
            || !mark_func.params.is_empty()
        {
            return None;
        }
        let chunk_for_ic = |candidate: Option<&Option<Rc<Chunk>>>| {
            candidate
                .and_then(|c| c.as_ref())
                .filter(|c| Rc::as_ptr(c) == mark_ic.chunk_raw)
                .cloned()
        };
        let mark_chunk = chunk_for_ic(mark_func.code2.get())
            .or_else(|| chunk_for_ic(mark_func.code.get()))?;
        let [
            Op::GetPropThis(state_name0, state_cache),
            Op::LoadName(_, runnable_cache),
            Op::BitOr,
            Op::SetPropThisDrop(state_name1, state_store),
            Op::ReturnUndef,
        ] = mark_chunk.jit_ops()
        else {
            return None;
        };
        let state = mark_chunk.jit_cache_preferred(*state_cache)?;
        if state_name0 != state_name1
            || state.depth != 0
            || !same_own(&mark_chunk, *state_store, state)
            || mark_chunk
                .jit_name_number(*runnable_cache)
                .and_then(exact_i32_const)
                .is_none()
        {
            return None;
        }
        Some((
            mark_obj,
            state,
            mark_chunk.jit_name_cache_ptr(*runnable_cache),
        ))
    };
    let mut mark_info = parse_mark(chunk, *mark_call);
    if mark_info.is_none() {
        let check_obj = check_target.pin.upgrade()?;
        let check_func = match &check_obj.borrow().call {
            crate::value::Callable::User(user) => Rc::clone(&user.func),
            _ => return None,
        };
        if check_func.is_arrow
            || check_func.is_generator
            || check_func.is_async
            || check_func.params.len() != 2
        {
            return None;
        }
        for check_chunk in [
            check_func.code2.get().and_then(|c| c.as_ref()),
            check_func.code.get().and_then(|c| c.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            let [
                Op::GetPropThis(queue0, _),
                Op::Const(null0),
                Op::EqEq,
                Op::JumpIfFalse(nonempty),
                Op::LoadLocal(packet0),
                Op::SetPropThisDrop(queue1, _),
                Op::LoadThis,
                Op::GetMethod(_, _),
                Op::CallWithThis(0, child_mark_call),
                Op::Pop,
                Op::GetPropThis(priority0, _),
                Op::GetPropLocal(task0, priority1, _),
                Op::Gt,
                Op::JumpIfFalse(no_preempt0),
                Op::LoadThis,
                Op::Return,
                Op::Jump(return_current0),
                Op::LoadLocal(packet1),
                Op::GetMethod(_, _),
                Op::GetPropThis(queue2, _),
                Op::CallWithThis(1, _),
                Op::SetPropThisDrop(queue3, _),
                Op::LoadLocal(task1),
                Op::Return,
                Op::ReturnUndef,
            ] = check_chunk.jit_ops()
            else {
                continue;
            };
            if queue0 != queue1
                || queue0 != queue2
                || queue0 != queue3
                || priority0 != priority1
                || packet0 != packet1
                || task0 != task1
                || *packet0 != 1
                || *task0 != 0
                || *nonempty != 17
                || *no_preempt0 != 16
                || *return_current0 != 22
                || !check_chunk.jit_const_copyable(*null0)
                || check_chunk.jit_const_bits(*null0) != (2, 0)
            {
                continue;
            }
            mark_info = parse_mark(check_chunk, *child_mark_call);
            if mark_info.is_some() {
                break;
            }
        }
    }
    let (mark_obj, state, runnable_cache) = mark_info?;

    let shapes = [
        v1.recv_shape,
        blocks.recv_shape,
        packet_id.recv_shape,
        target_queue.recv_shape,
    ];
    if v1.depth != 0
        || !same_own(chunk, *v1_cache1, v1)
        || !same_own(chunk, *v1_store, v1)
        || handler_scheduler.depth != 0
        || handler_scheduler.recv_shape != v1.recv_shape
        || handler_scheduler.slot == v1.slot
        || queue_method.depth != 1
        || queue_method.recv_shape != blocks.recv_shape
        || blocks.depth != 0
        || handler_scheduler.slot == v1.slot
        || queue_count.depth != 0
        || queue_count.recv_shape != blocks.recv_shape
        || current_id.depth != 0
        || current_id.recv_shape != blocks.recv_shape
        || current.depth != 0
        || current.recv_shape != blocks.recv_shape
        || !same_own(chunk, outer_current_store, current)
        || packet_id.depth != 0
        || packet_link.depth != 0
        || packet_link.recv_shape != packet_id.recv_shape
        || !same_own(chunk, *packet_id_store, packet_id)
        || !same_own(chunk, *packet_link_store, packet_link)
        || check_method.depth != 1
        || check_method.recv_shape != target_queue.recv_shape
        || target_queue.depth != 0
        || !same_own(chunk, *target_queue_store, target_queue)
        || mark_method.depth != 1
        || mark_method.recv_shape != target_queue.recv_shape
        || state.depth != 0
        || state.recv_shape != target_queue.recv_shape
        || target_priority.depth != 0
        || target_priority.recv_shape != target_queue.recv_shape
        || current_priority.depth != 0
        || current_priority.recv_shape != target_queue.recv_shape
        || current_priority.slot != target_priority.slot
        || !distinct_slots(&[blocks, queue_count, current_id, current])
        || !distinct_slots(&[packet_id, packet_link])
        || !distinct_slots(&[target_queue, state, target_priority])
        || shapes
            .iter()
            .enumerate()
            .any(|(i, shape)| shapes[..i].contains(shape))
    {
        return None;
    }
    let queue_expected = expected(queue_target)?;
    let check_expected = expected(check_target)?;
    let mark_expected = {
        let some: Option<crate::value::Gc> = Some(mark_obj);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    Some(SchedulerHandlerQueuePlan {
        handler_off,
        v1,
        queue: SchedulerDeviceQueuePlan {
            loop_pc,
            scheduler: handler_scheduler,
            queue_method,
            queue_expected,
            blocks,
            packet_id,
            queue_count,
            packet_link,
            current_id,
            check_method,
            check_expected,
            current,
            target_queue,
            mark_method,
            mark_expected,
            state,
            runnable_cache,
            target_priority,
            current_priority,
        },
    })
}

/// Recognize HandlerTask's numeric delivery arm after the live `v2 != null` branch. The lowering
/// covers Scheduler.queue's empty/preempting case and its dominant nonempty one-node case, where
/// Packet.addTo performs no scan and appends directly to that packet's Null link.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_handler_deliver(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerHandlerDeliverPlan> {
    use crate::bytecode::{Op, UpdKind};
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_DELIVER").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !set_prop_inlinable(layout)
        || !get_method_inlinable(layout)
        || !elem_inlinable(layout)
        || !packed_elem_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
        || cfg.stack_depth_at(head) != Some(0)
    {
        return None;
    }

    let end = head.checked_add(80)?;
    let [
        Op::GetPropLocal(handler, v2_name0, v2_cache0),
        Op::StoreLocal(saved),
        Op::GetPropLocal(handler0, v2_name1, v2_cache1),
        Op::GetProp(link_name0, packet_link_cache),
        Op::SetPropLocalDrop(handler1, v2_name2, v2_store),
        Op::GetPropLocal(handler2, v1_name0, v1_cache0),
        Op::GetProp(_, payload_array_cache),
        Op::LoadLocal(count),
        Op::GetElem,
        Op::SetPropLocalDrop(saved0, a1_name0, packet_a1_store),
        Op::GetPropLocal(handler3, v1_name1, v1_cache1),
        Op::LoadLocal(count0),
        Op::Const(one),
        Op::Add,
        Op::SetPropDrop(a1_name1, work_a1_store),
        Op::GetPropLocal(handler4, _, scheduler_cache),
        Op::GetMethod(_, queue_method_cache),
        Op::LoadLocal(saved1),
        Op::InlineGuard(queue_target, queue_generic),
        Op::StoreLocal(packet),
        Op::Pop,
        Op::StoreLocal(scheduler),
        Op::ResetSlots(target, reset_count),
        Op::GetPropLocal(scheduler0, _, blocks_cache),
        Op::GetPropLocal(packet0, id_name0, packet_id_cache),
        Op::GetElem,
        Op::StoreLocal(target0),
        Op::LoadLocal(target1),
        Op::Const(null_target),
        Op::EqEq,
        Op::JumpIfFalse(nonnull_target),
        Op::LoadLocal(target2),
        Op::Jump(null_return),
        Op::LoadLocal(scheduler1),
        Op::UpdateProp(_, queue_count_cache, UpdKind::IncDiscard),
        Op::Const(null_link),
        Op::SetPropLocalDrop(packet1, link_name1, packet_link_store),
        Op::GetPropLocal(scheduler2, _, current_id_cache),
        Op::SetPropLocalDrop(packet2, id_name1, packet_id_store),
        Op::LoadLocal(target3),
        Op::GetMethod(_, check_method_cache),
        Op::GetPropLocal(scheduler3, current_name0, current_cache),
        Op::LoadLocal(packet3),
        Op::InlineGuard(check_target, check_generic),
        Op::StoreLocal(check_packet),
        Op::StoreLocal(check_task),
        Op::Pop,
        Op::StoreLocal(check_this),
        Op::GetPropLocal(check_this0, queue_name0, target_queue_cache0),
        Op::Const(null_queue),
        Op::EqEq,
        Op::JumpIfFalse(nonempty_queue),
        Op::LoadLocal(check_packet0),
        Op::SetPropLocalDrop(check_this1, queue_name1, target_queue_store0),
        Op::LoadLocal(check_this2),
        Op::GetMethod(_, mark_method_cache),
        Op::CallWithThis(0, _mark_call),
        Op::Pop,
        Op::GetPropLocal(check_this3, priority_name0, target_priority_cache),
        Op::GetPropLocal(check_task0, priority_name1, current_priority_cache),
        Op::Gt,
        Op::JumpIfFalse(no_preempt),
        Op::LoadLocal(check_this4),
        Op::Jump(return_join0),
        Op::Jump(return_current),
        Op::LoadLocal(check_packet1),
        Op::GetMethod(_, _outer_add_method_cache),
        Op::GetPropLocal(check_this5, queue_name2, target_queue_cache1),
        Op::CallWithThis(1, _outer_add_call),
        Op::SetPropLocalDrop(check_this6, queue_name3, target_queue_store1),
        Op::LoadLocal(check_task1),
        Op::Jump(return_join1),
        Op::Undef,
        Op::Jump(return_join2),
        Op::CallWithThis(2, _),
        Op::Jump(queue_join0),
        Op::Undef,
        Op::Jump(queue_join1),
        Op::CallWithThis(1, _),
        Op::Jump(outer_join),
    ] = ops.get(head..end)?
    else {
        return None;
    };
    let after_call = match ops.get(*outer_join as usize) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let (current_name1, outer_current_store) = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(name, cache)) => (*name, *cache),
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if cfg.stack_depth_at(loop_pc) != Some(0) {
        return None;
    }
    if handler != handler0
        || handler != handler1
        || handler != handler2
        || handler != handler3
        || handler != handler4
        || v2_name0 != v2_name1
        || v2_name0 != v2_name2
        || v1_name0 != v1_name1
        || a1_name0 != a1_name1
        || link_name0 != link_name1
        || id_name0 != id_name1
        || current_name0 != &current_name1
        || queue_name0 != queue_name1
        || queue_name0 != queue_name2
        || queue_name0 != queue_name3
        || priority_name0 != priority_name1
        || saved != saved0
        || saved != saved1
        || count != count0
        || scheduler != scheduler0
        || scheduler != scheduler1
        || scheduler != scheduler2
        || scheduler != scheduler3
        || packet != packet0
        || packet != packet1
        || packet != packet2
        || packet != packet3
        || target != target0
        || target != target1
        || target != target2
        || target != target3
        || check_packet != check_packet0
        || check_packet != check_packet1
        || check_task != check_task0
        || check_task != check_task1
        || check_this != check_this0
        || check_this != check_this1
        || check_this != check_this2
        || check_this != check_this3
        || check_this != check_this4
        || check_this != check_this5
        || check_this != check_this6
        || *reset_count != 1
        || *queue_generic as usize != head + 78
        || *nonnull_target as usize != head + 33
        || *null_return as usize != head + 79
        || *check_generic as usize != head + 74
        || *nonempty_queue as usize != head + 65
        || *no_preempt as usize != head + 64
        || *return_join0 as usize != head + 75
        || *return_current as usize != head + 70
        || *return_join1 as usize != head + 75
        || *return_join2 as usize != head + 75
        || *queue_join0 as usize != head + 79
        || *queue_join1 as usize != head + 79
        || after_call != *outer_join as usize + 4
        || chunk.jit_const_num(*one).and_then(exact_i32_const) != Some(1)
        || ![null_target, null_link, null_queue]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }
    let locals = [
        *handler,
        *count,
        *saved,
        *packet,
        *scheduler,
        *target,
        *check_packet,
        *check_task,
        *check_this,
    ];
    if locals
        .iter()
        .enumerate()
        .any(|(i, slot)| locals[..i].contains(slot))
    {
        return None;
    }
    let handler_off = *handler as u32 * 16;
    let count_off = *count as u32 * 16;
    let saved_off = *saved as u32 * 16;
    if handler_off + 16 >= 4096 || count_off + 16 >= 4096 || saved_off + 16 >= 4096 {
        return None;
    }

    let v2 = chunk.jit_cache_preferred(*v2_cache0)?;
    let v1 = chunk.jit_cache_preferred(*v1_cache0)?;
    let payload_array = chunk.jit_cache_preferred(*payload_array_cache)?;
    let packet_a1 = chunk.jit_cache_preferred(*packet_a1_store)?;
    let work_a1 = chunk.jit_cache_preferred(*work_a1_store)?;
    let handler_scheduler = chunk.jit_cache_preferred(*scheduler_cache)?;
    let queue_method = chunk.jit_cache_preferred(*queue_method_cache)?;
    let blocks = chunk.jit_cache_preferred(*blocks_cache)?;
    let packet_id = chunk.jit_cache_preferred(*packet_id_cache)?;
    let packet_link = chunk.jit_cache_preferred(*packet_link_cache)?;
    let queue_count = chunk.jit_cache_preferred(*queue_count_cache)?;
    let current_id = chunk.jit_cache_preferred(*current_id_cache)?;
    let check_method = chunk.jit_cache_preferred(*check_method_cache)?;
    let current = chunk.jit_cache_preferred(*current_cache)?;
    let target_queue = chunk.jit_cache_preferred(*target_queue_cache0)?;
    let mark_method = chunk.jit_cache_preferred(*mark_method_cache)?;
    let target_priority = chunk.jit_cache_preferred(*target_priority_cache)?;
    let current_priority = chunk.jit_cache_preferred(*current_priority_cache)?;
    let same_own = |owner: &Chunk, cache: u32, expected: crate::bytecode::IcState| {
        owner.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let distinct_slots = |states: &[crate::bytecode::IcState]| {
        states.iter().enumerate().all(|(i, state)| {
            states[..i]
                .iter()
                .all(|earlier| earlier.slot != state.slot)
        })
    };
    let queue_target = chunk.jit_inline_target(*queue_target);
    let check_target = chunk.jit_inline_target(*check_target);
    if queue_target.argc != 1
        || !queue_target.check_this
        || check_target.argc != 2
        || !check_target.check_this
    {
        return None;
    }
    let gc_raw = |obj: &crate::value::Gc| {
        let some: Option<crate::value::Gc> = Some(Rc::clone(obj));
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    let queue_obj = queue_target.pin.upgrade()?;
    let check_obj = check_target.pin.upgrade()?;
    let queue_expected = gc_raw(&queue_obj);
    let check_expected = gc_raw(&check_obj);

    // Recover the exact markAsRunnable and Packet.addTo callees from the warmed source
    // checkPriorityAdd chunk. The scheduler's newly-created nested CallWithThis caches can still
    // be empty when this second-stage region is compiled.
    let check_func = match &check_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    if check_func.is_arrow
        || check_func.is_generator
        || check_func.is_async
        || check_func.params.len() != 2
    {
        return None;
    }

    let parse_mark = |owner: &Chunk,
                      call: u32|
     -> Option<(crate::value::Gc, crate::bytecode::IcState, usize)> {
        let (mark_ic, mark_obj) = owner.jit_call_target(call)?;
        if mark_ic.native != 0 || mark_ic.chunk_raw.is_null() {
            return None;
        }
        let mark_func = match &mark_obj.borrow().call {
            crate::value::Callable::User(user) => Rc::clone(&user.func),
            _ => return None,
        };
        if mark_func.is_arrow
            || mark_func.is_generator
            || mark_func.is_async
            || !mark_func.params.is_empty()
        {
            return None;
        }
        let mark_chunk = [
            mark_func.code2.get().and_then(|c| c.as_ref()),
            mark_func.code.get().and_then(|c| c.as_ref()),
        ]
        .into_iter()
        .flatten()
        .find(|candidate| Rc::as_ptr(candidate) == mark_ic.chunk_raw)?;
        let [
            Op::GetPropThis(state_name0, state_cache),
            Op::LoadName(_, runnable_cache),
            Op::BitOr,
            Op::SetPropThisDrop(state_name1, state_store),
            Op::ReturnUndef,
        ] = mark_chunk.jit_ops()
        else {
            return None;
        };
        let state = mark_chunk.jit_cache_preferred(*state_cache)?;
        if state_name0 != state_name1
            || state.depth != 0
            || !same_own(mark_chunk, *state_store, state)
            || mark_chunk
                .jit_name_number(*runnable_cache)
                .and_then(exact_i32_const)
                .is_none()
        {
            return None;
        }
        Some((
            mark_obj,
            state,
            mark_chunk.jit_name_cache_ptr(*runnable_cache),
        ))
    };
    let mut add_info = None;
    for check_chunk in [
        check_func.code2.get().and_then(|c| c.as_ref()),
        check_func.code.get().and_then(|c| c.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        let [
            Op::GetPropThis(queue0, _),
            Op::Const(null0),
            Op::EqEq,
            Op::JumpIfFalse(nonempty0),
            Op::LoadLocal(packet4),
            Op::SetPropThisDrop(queue1, _),
            Op::LoadThis,
            Op::GetMethod(_, _),
            Op::CallWithThis(0, child_mark_call),
            Op::Pop,
            Op::GetPropThis(priority0, _),
            Op::GetPropLocal(task4, priority1, _),
            Op::Gt,
            Op::JumpIfFalse(no_preempt0),
            Op::LoadThis,
            Op::Return,
            Op::Jump(return_current0),
            Op::LoadLocal(packet5),
            Op::GetMethod(_, add_method_cache),
            Op::GetPropThis(queue2, _),
            Op::CallWithThis(1, add_call),
            Op::SetPropThisDrop(queue3, _),
            Op::LoadLocal(task5),
            Op::Return,
            Op::ReturnUndef,
        ] = check_chunk.jit_ops()
        else {
            continue;
        };
        if queue0 != queue1
            || queue0 != queue2
            || queue0 != queue3
            || priority0 != priority1
            || packet4 != packet5
            || task4 != task5
            || *packet4 != 1
            || *task4 != 0
            || *nonempty0 != 17
            || *no_preempt0 != 16
            || *return_current0 != 22
            || !check_chunk.jit_const_copyable(*null0)
            || check_chunk.jit_const_bits(*null0) != (2, 0)
        {
            continue;
        }
        let Some(mark_info) = parse_mark(check_chunk, *child_mark_call) else {
            continue;
        };
        let Some(add_method) = check_chunk.jit_cache_preferred(*add_method_cache) else {
            continue;
        };
        let Some((add_ic, add_obj)) = check_chunk.jit_call_target(*add_call) else {
            continue;
        };
        if add_ic.native != 0 || add_ic.chunk_raw.is_null() {
            continue;
        }
        let add_func = match &add_obj.borrow().call {
            crate::value::Callable::User(user) => Rc::clone(&user.func),
            _ => continue,
        };
        if add_func.is_arrow
            || add_func.is_generator
            || add_func.is_async
            || add_func.params.len() != 1
        {
            continue;
        }
        let add_chunk = [
            add_func.code2.get().and_then(|c| c.as_ref()),
            add_func.code.get().and_then(|c| c.as_ref()),
        ]
        .into_iter()
        .flatten()
        .find(|c| Rc::as_ptr(c) == add_ic.chunk_raw)
        .cloned();
        let Some(add_chunk) = add_chunk else {
            continue;
        };
        let [
            Op::Const(add_null0),
            Op::SetPropThisDrop(add_link0, this_link_store),
            Op::LoadLocal(add_queue0),
            Op::Const(add_null1),
            Op::EqEq,
            Op::JumpIfFalse(add_nonempty),
            Op::LoadThis,
            Op::Return,
            Op::LoadLocal(add_queue1),
            Op::StoreLocal(next),
            Op::GetPropLocal(next0, add_link1, queued_link_cache),
            Op::Dup,
            Op::StoreLocal(peek),
            Op::Const(add_null2),
            Op::NotEq,
            Op::JumpIfFalse(scan_done),
            Op::LoadLocal(peek0),
            Op::StoreLocal(next1),
            Op::Jump(scan_head),
            Op::LoadThis,
            Op::SetPropLocalDrop(next2, add_link2, queued_link_store),
            Op::LoadLocal(add_queue2),
            Op::Return,
            Op::ReturnUndef,
        ] = add_chunk.jit_ops()
        else {
            continue;
        };
        if add_link0 != add_link1
            || add_link0 != add_link2
            || add_queue0 != add_queue1
            || add_queue0 != add_queue2
            || next != next0
            || next != next1
            || next != next2
            || peek != peek0
            || *add_queue0 != 0
            || *add_nonempty != 8
            || *scan_done != 19
            || *scan_head != 10
            || ![add_null0, add_null1, add_null2]
                .into_iter()
                .all(|k| add_chunk.jit_const_copyable(*k) && add_chunk.jit_const_bits(*k) == (2, 0))
        {
            continue;
        }
        let Some(this_link) = add_chunk.jit_cache_preferred(*this_link_store) else {
            continue;
        };
        let Some(queued_link) = add_chunk.jit_cache_preferred(*queued_link_cache) else {
            continue;
        };
        if this_link.depth != 0
            || queued_link.depth != 0
            || !same_own(&add_chunk, *queued_link_store, queued_link)
            || this_link.recv_shape != queued_link.recv_shape
            || this_link.slot != queued_link.slot
        {
            continue;
        }
        add_info = Some((add_method, gc_raw(&add_obj), queued_link, mark_info));
        break;
    }
    let (add_method, add_expected, queued_link, (mark_obj, state, runnable_cache)) = add_info?;
    let mark_expected = gc_raw(&mark_obj);

    let shapes = [
        v1.recv_shape,
        blocks.recv_shape,
        packet_id.recv_shape,
        target_queue.recv_shape,
    ];
    if v2.depth != 0
        || v1.depth != 0
        || v1.recv_shape != v2.recv_shape
        || v1.slot == v2.slot
        || !same_own(chunk, *v2_cache1, v2)
        || !same_own(chunk, *v2_store, v2)
        || !same_own(chunk, *v1_cache1, v1)
        || payload_array.depth != 0
        || payload_array.recv_shape != packet_id.recv_shape
        || packet_a1.depth != 0
        || work_a1.depth != 0
        || packet_a1.recv_shape != packet_id.recv_shape
        || work_a1.recv_shape != packet_id.recv_shape
        || packet_a1.slot != work_a1.slot
        || handler_scheduler.depth != 0
        || handler_scheduler.recv_shape != v1.recv_shape
        || !distinct_slots(&[v1, v2, handler_scheduler])
        || queue_method.depth != 1
        || queue_method.recv_shape != blocks.recv_shape
        || blocks.depth != 0
        || queue_count.depth != 0
        || queue_count.recv_shape != blocks.recv_shape
        || current_id.depth != 0
        || current_id.recv_shape != blocks.recv_shape
        || current.depth != 0
        || current.recv_shape != blocks.recv_shape
        || !same_own(chunk, outer_current_store, current)
        || packet_id.depth != 0
        || packet_link.depth != 0
        || packet_link.recv_shape != packet_id.recv_shape
        || !same_own(chunk, *packet_id_store, packet_id)
        || !same_own(chunk, *packet_link_store, packet_link)
        || check_method.depth != 1
        || check_method.recv_shape != target_queue.recv_shape
        || target_queue.depth != 0
        || !same_own(chunk, *target_queue_cache1, target_queue)
        || !same_own(chunk, *target_queue_store0, target_queue)
        || !same_own(chunk, *target_queue_store1, target_queue)
        || mark_method.depth != 1
        || mark_method.recv_shape != target_queue.recv_shape
        || state.depth != 0
        || state.recv_shape != target_queue.recv_shape
        || target_priority.depth != 0
        || target_priority.recv_shape != target_queue.recv_shape
        || current_priority.depth != 0
        || current_priority.recv_shape != target_queue.recv_shape
        || current_priority.slot != target_priority.slot
        || add_method.depth != 1
        || add_method.recv_shape != packet_id.recv_shape
        || queued_link.depth != 0
        || queued_link.recv_shape != packet_id.recv_shape
        || queued_link.slot != packet_link.slot
        || !distinct_slots(&[blocks, queue_count, current_id, current])
        || !distinct_slots(&[packet_id, packet_link, packet_a1, payload_array])
        || !distinct_slots(&[target_queue, state, target_priority])
        || shapes
            .iter()
            .enumerate()
            .any(|(i, shape)| shapes[..i].contains(shape))
    {
        return None;
    }
    Some(SchedulerHandlerDeliverPlan {
        handler_off,
        count_off,
        saved_off,
        loop_pc,
        empty_preempt: std::env::var_os("LUMEN_JIT_NO_SCHED_HANDLER_DELIVER_EMPTY").is_none(),
        v1,
        v2,
        payload_array,
        packet_a1,
        work_a1,
        scheduler: handler_scheduler,
        queue_method,
        queue_expected,
        blocks,
        packet_id,
        queue_count,
        packet_link,
        current_id,
        check_method,
        check_expected,
        current,
        target_queue,
        mark_method,
        mark_expected,
        state,
        runnable_cache,
        target_priority,
        current_priority,
        add_method,
        add_expected,
        queued_link,
    })
}

/// Recognize the virtual task call at the active-prefix exit and the inlined DeviceTask body.
/// The first HandlerTask inline guard deliberately falls through to a second DeviceTask guard;
/// property IC way selection therefore keys off DeviceTask's own `v1` shape instead of requiring
/// the polymorphic `run` site to collapse to one preferred way.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_device(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<SchedulerDevicePlan> {
    use crate::bytecode::Op;
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_REGION").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || !get_method_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        return None;
    }
    let outer_end = head.checked_add(4)?;
    let [
        Op::GetPropLocal(tcb, _, task_cache),
        Op::GetMethod(_, run_cache),
        Op::LoadLocal(packet),
        Op::InlineGuard(_, device_guard),
    ] = ops.get(head..outer_end)?
    else {
        return None;
    };
    let device_guard = *device_guard as usize;
    let device_end = device_guard.checked_add(33)?;
    let [
        Op::InlineGuard(device_target, generic_call),
        Op::StoreLocal(device_packet),
        Op::Pop,
        Op::StoreLocal(task),
        Op::ResetSlots(temp, reset_count),
        Op::LoadLocal(device_packet1),
        Op::Const(null_packet),
        Op::EqEq,
        Op::JumpIfFalse(hold_pc),
        Op::GetPropLocal(task1, _, v1_cache0),
        Op::Const(null_v1),
        Op::EqEq,
        Op::JumpIfFalse(queue_pc),
        Op::GetPropLocal(task2, scheduler_name0, scheduler_cache0),
        Op::GetMethod(suspend_name, _),
        Op::CallWithThis(0, _),
        Op::Jump(join0),
        Op::GetPropLocal(task3, _, v1_cache1),
        Op::StoreLocal(temp1),
        Op::Const(null_store),
        Op::SetPropLocalDrop(task4, _, v1_store0),
        Op::GetPropLocal(task5, scheduler_name1, _scheduler_cache1),
        Op::GetMethod(_queue_name, _queue_method_cache),
        Op::LoadLocal(temp2),
        Op::CallWithThis(1, _queue_call),
        Op::Jump(join1),
        Op::Jump(scaffold),
        Op::LoadLocal(device_packet2),
        Op::SetPropLocalDrop(task6, _, v1_store1),
        Op::GetPropLocal(task7, scheduler_name2, scheduler_cache2),
        Op::GetMethod(hold_name, hold_method_cache),
        Op::CallWithThis(0, _),
        Op::Jump(join2),
    ] = ops.get(device_guard..device_end)?
    else {
        return None;
    };
    let suspend_pc = device_guard + 13;
    let expected_join = device_guard + 36;
    if *device_packet != *device_packet1
        || *device_packet != *device_packet2
        || *task != *task1
        || *task != *task2
        || *task != *task3
        || *task != *task4
        || *task != *task5
        || *task != *task6
        || *task != *task7
        || *temp != *temp1
        || *temp != *temp2
        || scheduler_name0 != scheduler_name1
        || scheduler_name0 != scheduler_name2
        || *reset_count != 1
        || *hold_pc as usize != device_guard + 27
        || *queue_pc as usize != device_guard + 17
        || *generic_call as usize != device_guard + 35
        || *join0 as usize != expected_join
        || *join1 as usize != expected_join
        || *join2 as usize != expected_join
        || *scaffold as usize != device_guard + 33
        || !matches!(ops.get(device_guard + 33), Some(Op::Undef))
        || !matches!(ops.get(device_guard + 34), Some(Op::Jump(t)) if *t as usize == expected_join)
        || ![null_packet, null_v1, null_store]
            .into_iter()
            .all(|k| chunk.jit_const_copyable(*k) && chunk.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }
    let target = chunk.jit_inline_target(*device_target);
    if target.argc != 1 || !target.check_this {
        return None;
    }
    let device_obj = target.pin.upgrade()?;
    let run_expected = {
        let some: Option<crate::value::Gc> = Some(Rc::clone(&device_obj));
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    let task_state = chunk.jit_cache_preferred(*task_cache)?;
    let v1 = chunk.jit_cache_preferred(*v1_cache0)?;
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    if task_state.depth != 0
        || v1.depth != 0
        || !same_own(*v1_cache1, v1)
        || !same_own(*v1_store0, v1)
        || !same_own(*v1_store1, v1)
    {
        return None;
    }
    let run_method = chunk.jit_cache_for_shape(*run_cache, v1.recv_shape)?;
    if run_method.depth != 1 || run_method.recv_shape != v1.recv_shape {
        return None;
    }
    let slots = [*tcb, *packet, *device_packet, *task, *temp];
    if slots
        .iter()
        .enumerate()
        .any(|(i, slot)| slots[..i].contains(slot) || (*slot as u32) * 16 + 16 >= 4096)
    {
        return None;
    }
    let suspend = plan_scheduler_device_suspend(
        chunk,
        ops,
        device_guard,
        *scheduler_name0,
        *scheduler_cache0,
        *suspend_name,
        task_state.recv_shape,
        v1.recv_shape,
        expected_join,
    );
    let queue = plan_scheduler_device_queue(
        chunk,
        ops,
        &device_obj,
        *_scheduler_cache1,
        *_queue_name,
        *_queue_method_cache,
        task_state.recv_shape,
        v1,
        expected_join,
        layout,
    );
    let hold = plan_scheduler_device_hold(
        chunk,
        ops,
        &device_obj,
        *scheduler_cache2,
        *hold_name,
        *hold_method_cache,
        task_state.recv_shape,
        v1.recv_shape,
        expected_join,
    );
    Some(SchedulerDevicePlan {
        suspend_pc,
        queue_pc: *queue_pc as usize,
        hold_pc: *hold_pc as usize,
        tcb_off: *tcb as u32 * 16,
        packet_off: *packet as u32 * 16,
        device_packet_off: *device_packet as u32 * 16,
        task_off: *task as u32 * 16,
        temp_off: *temp as u32 * 16,
        task: task_state,
        run_method,
        run_expected,
        v1,
        suspend,
        queue,
        hold,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[allow(clippy::too_many_arguments)]
fn plan_scheduler_device_suspend(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    device_guard: usize,
    scheduler_name: u32,
    scheduler_cache: u32,
    suspend_name: u32,
    tcb_shape: u32,
    device_shape: u32,
    expected_join: usize,
) -> Option<SchedulerDeviceSuspendPlan> {
    use crate::bytecode::Op;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some() {
        return None;
    }
    // The same scheduler method is inlined in the HandlerTask arm immediately preceding the
    // Device guard. That gives this region stable, pinned identities for both suspendCurrent and
    // its nested markAsSuspended call; the Device call site itself remains generic.
    let start = device_guard.checked_sub(26)?;
    let end = start.checked_add(24)?;
    let [
        Op::GetPropLocal(_, scheduler_name0, _),
        Op::GetMethod(suspend_name0, suspend_method_cache),
        Op::InlineGuard(suspend_target, suspend_generic),
        Op::Pop,
        Op::StoreLocal(scheduler_local),
        Op::GetPropLocal(scheduler_local0, current_name0, current_cache0),
        Op::GetMethod(_, mark_method_cache),
        Op::InlineGuard(mark_target, mark_generic),
        Op::Pop,
        Op::StoreLocal(tcb_local),
        Op::GetPropLocal(tcb_local0, state_name0, state_cache0),
        Op::LoadName(_, suspended_cache),
        Op::BitOr,
        Op::SetPropLocalDrop(tcb_local1, state_name1, state_store),
        Op::Undef,
        Op::Jump(mark_join0),
        Op::CallWithThis(0, _),
        Op::Pop,
        Op::GetPropLocal(scheduler_local1, current_name1, current_cache1),
        Op::Jump(suspend_join0),
        Op::Undef,
        Op::Jump(suspend_join1),
        Op::CallWithThis(0, _),
        Op::Jump(join),
    ] = ops.get(start..end)?
    else {
        return None;
    };
    let after_call = match ops.get(expected_join) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let (outer_current_name, outer_current_store) = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(name, cache)) => (*name, *cache),
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if *scheduler_name0 != scheduler_name
        || *suspend_name0 != suspend_name
        || *scheduler_local != *scheduler_local0
        || *scheduler_local != *scheduler_local1
        || *tcb_local != *tcb_local0
        || *tcb_local != *tcb_local1
        || state_name0 != state_name1
        || current_name0 != current_name1
        || *current_name0 != outer_current_name
        || *suspend_generic as usize != start + 22
        || *mark_generic as usize != start + 16
        || *mark_join0 as usize != start + 17
        || *suspend_join0 as usize != start + 23
        || *suspend_join1 as usize != start + 23
        || *join as usize != expected_join
        || after_call != expected_join + 4
    {
        return None;
    }
    let scheduler = chunk.jit_cache_preferred(scheduler_cache)?;
    let suspend_method = chunk.jit_cache_preferred(*suspend_method_cache)?;
    let current = chunk.jit_cache_preferred(*current_cache0)?;
    let mark_method = chunk.jit_cache_preferred(*mark_method_cache)?;
    let state = chunk.jit_cache_preferred(*state_cache0)?;
    let same_own = |cache: u32, expected: crate::bytecode::IcState| {
        chunk.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    if scheduler.depth != 0
        || scheduler.recv_shape != device_shape
        || suspend_method.depth != 1
        || suspend_method.recv_shape != current.recv_shape
        || current.depth != 0
        || !same_own(*current_cache1, current)
        || !same_own(outer_current_store, current)
        || mark_method.depth != 1
        || mark_method.recv_shape != state.recv_shape
        || state.depth != 0
        || state.recv_shape != tcb_shape
        || !same_own(*state_store, state)
        || chunk
            .jit_name_number(*suspended_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let suspend_target = chunk.jit_inline_target(*suspend_target);
    let mark_target = chunk.jit_inline_target(*mark_target);
    if suspend_target.argc != 0
        || !suspend_target.check_this
        || mark_target.argc != 0
        || !mark_target.check_this
    {
        return None;
    }
    let expected = |target: &crate::bytecode::InlineTarget| {
        target.pin.upgrade().map(|o| {
            let some: Option<crate::value::Gc> = Some(o);
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        })
    };
    Some(SchedulerDeviceSuspendPlan {
        loop_pc,
        scheduler,
        suspend_method,
        suspend_expected: expected(suspend_target)?,
        current,
        mark_method,
        mark_expected: expected(mark_target)?,
        state,
        suspended_cache: chunk.jit_name_cache_ptr(*suspended_cache),
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[allow(clippy::too_many_arguments)]
fn plan_scheduler_device_hold(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    device_obj: &crate::value::Gc,
    scheduler_cache: u32,
    hold_name: u32,
    hold_method_cache: u32,
    tcb_shape: u32,
    device_shape: u32,
    expected_join: usize,
) -> Option<SchedulerDeviceHoldPlan> {
    use crate::bytecode::{Op, UpdKind};
    if std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_HOLD").is_some()
    {
        return None;
    }

    // The freshly emitted outer code2 call cache is empty at compile time. Follow the exact
    // Device.run object to its warmed original chunk instead: that call site has the monomorphic
    // holdCurrent target and pins it. Generated code still guards Device.run first, then this
    // exact hold method, before touching the hold chunk's nested target or name cache.
    let device_func = match &device_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    let device_chunk = device_func.code.get()?.as_ref()?;
    let device_ops = device_chunk.jit_ops();
    let [
        Op::LoadLocal(_),
        Op::Const(_),
        Op::EqEq,
        Op::JumpIfFalse(_),
        Op::GetPropThis(_, _),
        Op::Const(_),
        Op::EqEq,
        Op::JumpIfFalse(_),
        Op::GetPropThis(_, _),
        Op::GetMethod(_, _),
        Op::CallWithThis(0, _),
        Op::Return,
        Op::GetPropThis(_, _),
        Op::StoreLocal(_),
        Op::Const(_),
        Op::SetPropThisDrop(_, _),
        Op::GetPropThis(_, _),
        Op::GetMethod(_, _),
        Op::LoadLocal(_),
        Op::CallWithThis(1, _),
        Op::Return,
        Op::Jump(_),
        Op::LoadLocal(_),
        Op::SetPropThisDrop(_, _),
        Op::GetPropThis(_, _),
        Op::GetMethod(device_hold_name, _),
        Op::CallWithThis(0, device_hold_call),
        Op::Return,
        Op::ReturnUndef,
    ] = device_ops
    else {
        return None;
    };
    if chunk.jit_name(hold_name) != device_chunk.jit_name(*device_hold_name) {
        return None;
    }
    let hold_target = device_chunk.jit_call_target(*device_hold_call);
    let (hold_ic, hold_obj) = hold_target?;
    if hold_ic.native != 0 {
        return None;
    }
    let hold_expected = {
        let some: Option<crate::value::Gc> = Some(Rc::clone(&hold_obj));
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    let hold_func = match &hold_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    let hold_chunk = hold_func.code2.get()?.as_ref()?;
    let hold_ops = hold_chunk.jit_ops();
    let [
        Op::LoadThis,
        Op::UpdateProp(_, hold_count_cache, UpdKind::IncDiscard),
        Op::GetPropThis(current_name0, current_cache0),
        Op::GetMethod(_, mark_method_cache),
        Op::InlineGuard(mark_target, mark_generic),
        Op::Pop,
        Op::StoreLocal(mark_this),
        Op::GetPropLocal(mark_this0, state_name0, state_cache),
        Op::LoadName(_, held_cache),
        Op::BitOr,
        Op::SetPropLocalDrop(mark_this1, state_name1, state_store),
        Op::Undef,
        Op::Jump(mark_join),
        Op::CallWithThis(0, _),
        Op::Pop,
        Op::GetPropThis(current_name1, current_cache1),
        Op::GetProp(_, link_cache),
        Op::Return,
        Op::ReturnUndef,
    ] = hold_ops
    else {
        return None;
    };
    if *mark_this != *mark_this0
        || *mark_this != *mark_this1
        || state_name0 != state_name1
        || current_name0 != current_name1
        || *mark_generic != 13
        || *mark_join != 14
    {
        return None;
    }

    let after_call = match ops.get(expected_join) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let outer_current_store = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(_, cache)) => *cache,
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if after_call != expected_join + 4 {
        return None;
    }

    let scheduler = chunk.jit_cache_preferred(scheduler_cache)?;
    let hold_method = chunk.jit_cache_preferred(hold_method_cache)?;
    let hold_count = hold_chunk.jit_cache_preferred(*hold_count_cache)?;
    let current = hold_chunk.jit_cache_preferred(*current_cache0)?;
    let mark_method = hold_chunk.jit_cache_preferred(*mark_method_cache)?;
    let state = hold_chunk.jit_cache_preferred(*state_cache)?;
    let link = hold_chunk.jit_cache_preferred(*link_cache)?;
    let same_own = |owner: &Chunk, cache: u32, expected: crate::bytecode::IcState| {
        owner.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    if scheduler.depth != 0
        || scheduler.recv_shape != device_shape
        || hold_method.depth != 1
        || hold_method.recv_shape != current.recv_shape
        || hold_count.depth != 0
        || hold_count.recv_shape != current.recv_shape
        || current.depth != 0
        || !same_own(hold_chunk, *current_cache1, current)
        || !same_own(chunk, outer_current_store, current)
        || mark_method.depth != 1
        || mark_method.recv_shape != state.recv_shape
        || state.depth != 0
        || state.recv_shape != tcb_shape
        || !same_own(hold_chunk, *state_store, state)
        || link.depth != 0
        || link.recv_shape != tcb_shape
        || hold_chunk
            .jit_name_number(*held_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let mark_target = hold_chunk.jit_inline_target(*mark_target);
    if mark_target.argc != 0 || !mark_target.check_this {
        return None;
    }
    let mark_expected = mark_target.pin.upgrade().map(|o| {
        let some: Option<crate::value::Gc> = Some(o);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    })?;
    Some(SchedulerDeviceHoldPlan {
        loop_pc,
        scheduler,
        hold_method,
        hold_expected,
        hold_count,
        current,
        mark_method,
        mark_expected,
        state,
        held_cache: hold_chunk.jit_name_cache_ptr(*held_cache),
        link,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[allow(clippy::too_many_arguments)]
fn plan_scheduler_device_queue(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    device_obj: &crate::value::Gc,
    scheduler_cache: u32,
    queue_name: u32,
    queue_method_cache: u32,
    tcb_shape: u32,
    device_v1: crate::bytecode::IcState,
    expected_join: usize,
    layout: &crate::value::JitLayout,
) -> Option<SchedulerDeviceQueuePlan> {
    use crate::bytecode::{Op, UpdKind};
    let device_shape = device_v1.recv_shape;
    if std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT").is_some()
        || std::env::var_os("LUMEN_JIT_NO_SCHED_DEVICE_QUEUE").is_some()
        || !elem_inlinable(layout)
        || !packed_elem_inlinable(layout)
    {
        return None;
    }
    if let Some(plan) = plan_scheduler_device_queue_code2(
        chunk,
        ops,
        device_obj,
        tcb_shape,
        device_v1,
        expected_join,
    ) {
        return Some(plan);
    }

    let device_func = match &device_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    let device_chunk = device_func.code.get()?.as_ref()?;
    let device_ops = device_chunk.jit_ops();
    let [
        Op::LoadLocal(_),
        Op::Const(_),
        Op::EqEq,
        Op::JumpIfFalse(_),
        Op::GetPropThis(_, _),
        Op::Const(_),
        Op::EqEq,
        Op::JumpIfFalse(_),
        Op::GetPropThis(_, _),
        Op::GetMethod(_, _),
        Op::CallWithThis(0, _),
        Op::Return,
        Op::GetPropThis(_, _),
        Op::StoreLocal(_),
        Op::Const(_),
        Op::SetPropThisDrop(_, _),
        Op::GetPropThis(_, _),
        Op::GetMethod(device_queue_name, _),
        Op::LoadLocal(_),
        Op::CallWithThis(1, device_queue_call),
        Op::Return,
        Op::Jump(_),
        Op::LoadLocal(_),
        Op::SetPropThisDrop(_, _),
        Op::GetPropThis(_, _),
        Op::GetMethod(_, _),
        Op::CallWithThis(0, _),
        Op::Return,
        Op::ReturnUndef,
    ] = device_ops
    else {
        return None;
    };
    if chunk.jit_name(queue_name) != device_chunk.jit_name(*device_queue_name) {
        return None;
    }
    let queue_target = device_chunk.jit_call_target(*device_queue_call);
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region-plan] scheduler Device queue: device_ops={}, call_target={}",
            device_ops.len(),
            queue_target.is_some()
        );
    }
    let (queue_ic, queue_obj) = queue_target?;
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region-plan] scheduler Device queue: native={}, chunk_null={}",
            queue_ic.native,
            queue_ic.chunk_raw.is_null()
        );
    }
    if queue_ic.native != 0 {
        return None;
    }
    let queue_expected = {
        let some: Option<crate::value::Gc> = Some(Rc::clone(&queue_obj));
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    };
    let queue_func = match &queue_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region-plan] scheduler Device queue: code2={}",
            queue_func.code2.get().and_then(|v| v.as_ref()).is_some()
        );
    }
    let queue_chunk = queue_func.code2.get()?.as_ref()?;
    let q = queue_chunk.jit_ops();
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!("[jit-region-plan] scheduler Device queue: queue_ops={}", q.len());
    }
    if q.len() != 93 {
        return None;
    }

    let [
        Op::GetPropThis(_, blocks_cache),
        Op::GetPropLocal(packet0, _, packet_id_cache),
        Op::GetElem,
        Op::StoreLocal(target),
        Op::LoadLocal(target0),
        Op::Const(null_target),
        Op::EqEq,
        Op::JumpIfFalse(nonnull_target),
        Op::LoadLocal(target1),
        Op::Return,
        Op::LoadThis,
        Op::UpdateProp(_, queue_count_cache, UpdKind::IncDiscard),
        Op::Const(null_link),
        Op::SetPropLocalDrop(packet1, _, packet_link_store),
        Op::GetPropThis(_, current_id_cache),
        Op::SetPropLocalDrop(packet2, _, packet_id_store),
        Op::LoadLocal(target2),
        Op::GetMethod(_, check_method_cache),
        Op::GetPropThis(_, current_cache),
        Op::LoadLocal(packet3),
        Op::InlineGuard(check_target, check_generic),
    ] = &q[..21]
    else {
        return None;
    };
    let [
        Op::StoreLocal(check_packet),
        Op::StoreLocal(check_task),
        Op::Pop,
        Op::StoreLocal(check_this),
        Op::GetPropLocal(check_this0, _, target_queue_cache),
        Op::Const(null_queue),
        Op::EqEq,
        Op::JumpIfFalse(nonempty_queue),
        Op::LoadLocal(check_packet0),
        Op::SetPropLocalDrop(check_this1, _, target_queue_store),
        Op::LoadLocal(check_this2),
        Op::GetMethod(_, mark_method_cache),
        Op::InlineGuard(mark_target, mark_generic),
        Op::Pop,
        Op::StoreLocal(mark_this),
        Op::GetPropLocal(mark_this0, _, state_cache),
        Op::LoadName(_, runnable_cache),
        Op::BitOr,
        Op::SetPropLocalDrop(mark_this1, _, state_store),
        Op::Undef,
        Op::Jump(mark_join),
        Op::CallWithThis(0, _),
        Op::Pop,
        Op::GetPropLocal(check_this3, _, target_priority_cache),
        Op::GetPropLocal(check_task0, _, current_priority_cache),
        Op::Gt,
        Op::JumpIfFalse(no_preempt),
        Op::LoadLocal(check_this4),
        Op::Jump(return_join0),
        Op::Jump(return_current),
    ] = &q[21..51]
    else {
        return None;
    };
    if *packet0 != *packet1
        || *packet0 != *packet2
        || *packet0 != *packet3
        || *target != *target0
        || *target != *target1
        || *target != *target2
        || *check_packet != *check_packet0
        || *check_this != *check_this0
        || *check_this != *check_this1
        || *check_this != *check_this2
        || *check_this != *check_this3
        || *check_this != *check_this4
        || *check_task != *check_task0
        || *mark_this != *mark_this0
        || *mark_this != *mark_this1
        || *nonnull_target != 10
        || *check_generic != 90
        || *nonempty_queue != 51
        || *mark_generic != 42
        || *mark_join != 43
        || *no_preempt != 50
        || *return_join0 != 91
        || *return_current != 86
        || !matches!(q.get(86), Some(Op::LoadLocal(s)) if s == check_task)
        || !matches!(q.get(87), Some(Op::Jump(91)))
        || !matches!(q.get(91), Some(Op::Return))
        || ![null_target, null_link, null_queue]
            .into_iter()
            .all(|k| queue_chunk.jit_const_copyable(*k) && queue_chunk.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }

    let after_call = match ops.get(expected_join) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let outer_current_store = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(_, cache)) => *cache,
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if after_call != expected_join + 4 {
        return None;
    }

    let scheduler = chunk.jit_cache_preferred(scheduler_cache)?;
    let queue_method = chunk.jit_cache_preferred(queue_method_cache)?;
    let blocks = queue_chunk.jit_cache_preferred(*blocks_cache)?;
    let packet_id = queue_chunk.jit_cache_preferred(*packet_id_cache)?;
    let queue_count = queue_chunk.jit_cache_preferred(*queue_count_cache)?;
    let packet_link = queue_chunk.jit_cache_preferred(*packet_link_store)?;
    let current_id = queue_chunk.jit_cache_preferred(*current_id_cache)?;
    let check_method = queue_chunk.jit_cache_preferred(*check_method_cache)?;
    let current = queue_chunk.jit_cache_preferred(*current_cache)?;
    let target_queue = queue_chunk.jit_cache_preferred(*target_queue_cache)?;
    let mark_method = queue_chunk.jit_cache_preferred(*mark_method_cache)?;
    let state = queue_chunk.jit_cache_preferred(*state_cache)?;
    let target_priority = queue_chunk.jit_cache_preferred(*target_priority_cache)?;
    let current_priority = queue_chunk.jit_cache_preferred(*current_priority_cache)?;
    let same_own = |owner: &Chunk, cache: u32, expected: crate::bytecode::IcState| {
        owner.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let distinct_slots = |states: &[crate::bytecode::IcState]| {
        states.iter().enumerate().all(|(i, state)| {
            states[..i]
                .iter()
                .all(|earlier| earlier.slot != state.slot)
        })
    };
    if scheduler.depth != 0
        || scheduler.recv_shape != device_shape
        || scheduler.slot == device_v1.slot
        || device_shape == tcb_shape
        || queue_method.depth != 1
        || queue_method.recv_shape != blocks.recv_shape
        || blocks.depth != 0
        || blocks.recv_shape == device_shape
        || blocks.recv_shape == tcb_shape
        || queue_count.depth != 0
        || queue_count.recv_shape != blocks.recv_shape
        || current_id.depth != 0
        || current_id.recv_shape != blocks.recv_shape
        || current.depth != 0
        || current.recv_shape != blocks.recv_shape
        || !same_own(chunk, outer_current_store, current)
        || packet_id.depth != 0
        || packet_link.depth != 0
        || packet_link.recv_shape != packet_id.recv_shape
        || !same_own(queue_chunk, *packet_id_store, packet_id)
        || packet_id.recv_shape == device_shape
        || packet_id.recv_shape == tcb_shape
        || packet_id.recv_shape == blocks.recv_shape
        || check_method.depth != 1
        || check_method.recv_shape != target_queue.recv_shape
        || target_queue.depth != 0
        || target_queue.recv_shape != tcb_shape
        || !same_own(queue_chunk, *target_queue_store, target_queue)
        || mark_method.depth != 1
        || mark_method.recv_shape != state.recv_shape
        || state.depth != 0
        || state.recv_shape != target_queue.recv_shape
        || !same_own(queue_chunk, *state_store, state)
        || target_priority.depth != 0
        || target_priority.recv_shape != target_queue.recv_shape
        || current_priority.depth != 0
        || current_priority.recv_shape != tcb_shape
        || current_priority.slot != target_priority.slot
        || !distinct_slots(&[blocks, queue_count, current_id, current])
        || !distinct_slots(&[packet_id, packet_link])
        || !distinct_slots(&[target_queue, state, target_priority])
        || queue_chunk
            .jit_name_number(*runnable_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let check_target = queue_chunk.jit_inline_target(*check_target);
    let mark_target = queue_chunk.jit_inline_target(*mark_target);
    if check_target.argc != 2
        || !check_target.check_this
        || mark_target.argc != 0
        || !mark_target.check_this
    {
        return None;
    }
    let expected = |target: &crate::bytecode::InlineTarget| {
        target.pin.upgrade().map(|o| {
            let some: Option<crate::value::Gc> = Some(o);
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        })
    };
    Some(SchedulerDeviceQueuePlan {
        loop_pc,
        scheduler,
        queue_method,
        queue_expected,
        blocks,
        packet_id,
        queue_count,
        packet_link,
        current_id,
        check_method,
        check_expected: expected(check_target)?,
        current,
        target_queue,
        mark_method,
        mark_expected: expected(mark_target)?,
        state,
        runnable_cache: queue_chunk.jit_name_cache_ptr(*runnable_cache),
        target_priority,
        current_priority,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_scheduler_device_queue_code2(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    device_obj: &crate::value::Gc,
    tcb_shape: u32,
    parent_v1: crate::bytecode::IcState,
    expected_join: usize,
) -> Option<SchedulerDeviceQueuePlan> {
    use crate::bytecode::{Op, UpdKind};
    let device_shape = parent_v1.recv_shape;
    let device_func = match &device_obj.borrow().call {
        crate::value::Callable::User(user) => Rc::clone(&user.func),
        _ => return None,
    };
    let d = device_func.code2.get()?.as_ref()?;
    let q = d.jit_ops();
    if q.len() != 171 {
        return None;
    }
    let [
        Op::GetPropThis(v1_name0, v1_cache),
        Op::StoreLocal(saved_v1),
        Op::Const(null_v1),
        Op::SetPropThisDrop(v1_name1, v1_store),
        Op::GetPropThis(_, scheduler_cache),
        Op::GetMethod(_, queue_method_cache),
        Op::LoadLocal(saved_v10),
        Op::InlineGuard(queue_target, queue_generic),
        Op::StoreLocal(packet),
        Op::Pop,
        Op::StoreLocal(scheduler),
        Op::ResetSlots(target, reset_count),
    ] = &q[32..44]
    else {
        return None;
    };
    let [
        Op::GetPropLocal(scheduler0, _, blocks_cache),
        Op::GetPropLocal(packet0, _, packet_id_cache),
        Op::GetElem,
        Op::StoreLocal(target0),
        Op::LoadLocal(target1),
        Op::Const(null_target),
        Op::EqEq,
        Op::JumpIfFalse(nonnull_target),
        Op::LoadLocal(target2),
        Op::Jump(null_return),
        Op::LoadLocal(scheduler1),
        Op::UpdateProp(_, queue_count_cache, UpdKind::IncDiscard),
        Op::Const(null_link),
        Op::SetPropLocalDrop(packet1, _, packet_link_store),
        Op::GetPropLocal(scheduler2, _, current_id_cache),
        Op::SetPropLocalDrop(packet2, _, packet_id_store),
        Op::LoadLocal(target3),
        Op::GetMethod(_, check_method_cache),
        Op::GetPropLocal(scheduler3, _, current_cache),
        Op::LoadLocal(packet3),
        Op::InlineGuard(check_target, check_generic),
    ] = &q[44..65]
    else {
        return None;
    };
    let [
        Op::StoreLocal(check_packet),
        Op::StoreLocal(check_task),
        Op::Pop,
        Op::StoreLocal(check_this),
        Op::GetPropLocal(check_this0, _, target_queue_cache),
        Op::Const(null_queue),
        Op::EqEq,
        Op::JumpIfFalse(nonempty_queue),
        Op::LoadLocal(check_packet0),
        Op::SetPropLocalDrop(check_this1, _, target_queue_store),
        Op::LoadLocal(check_this2),
        Op::GetMethod(_, mark_method_cache),
        Op::InlineGuard(mark_target, mark_generic),
        Op::Pop,
        Op::StoreLocal(mark_this),
        Op::GetPropLocal(mark_this0, state_name0, state_cache),
        Op::LoadName(_, runnable_cache),
        Op::BitOr,
        Op::SetPropLocalDrop(mark_this1, state_name1, state_store),
        Op::Undef,
        Op::Jump(mark_join),
        Op::CallWithThis(0, _),
        Op::Pop,
        Op::GetPropLocal(check_this3, _, target_priority_cache),
        Op::GetPropLocal(check_task0, _, current_priority_cache),
        Op::Gt,
        Op::JumpIfFalse(no_preempt),
        Op::LoadLocal(check_this4),
        Op::Jump(return_join0),
        Op::Jump(return_current),
    ] = &q[65..95]
    else {
        return None;
    };
    if v1_name0 != v1_name1
        || saved_v1 != saved_v10
        || *reset_count != 1
        || scheduler != scheduler0
        || scheduler != scheduler1
        || scheduler != scheduler2
        || scheduler != scheduler3
        || packet != packet0
        || packet != packet1
        || packet != packet2
        || packet != packet3
        || target != target0
        || target != target1
        || target != target2
        || target != target3
        || check_packet != check_packet0
        || check_this != check_this0
        || check_this != check_this1
        || check_this != check_this2
        || check_this != check_this3
        || check_this != check_this4
        || check_task != check_task0
        || mark_this != mark_this0
        || mark_this != mark_this1
        || state_name0 != state_name1
        || *queue_generic != 138
        || *nonnull_target != 54
        || *null_return != 139
        || *check_generic != 134
        || *nonempty_queue != 95
        || *mark_generic != 86
        || *mark_join != 87
        || *no_preempt != 94
        || *return_join0 != 135
        || *return_current != 130
        || !matches!(q.get(130), Some(Op::LoadLocal(s)) if s == check_task)
        || !matches!(q.get(131), Some(Op::Jump(135)))
        || !matches!(q.get(135), Some(Op::Jump(139)))
        || !matches!(q.get(139), Some(Op::Return))
        || ![null_v1, null_target, null_link, null_queue]
            .into_iter()
            .all(|k| d.jit_const_copyable(*k) && d.jit_const_bits(*k) == (2, 0))
    {
        return None;
    }

    let after_call = match ops.get(expected_join) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    let outer_current_store = match ops.get(after_call) {
        Some(Op::SetPropThisDrop(_, cache)) => *cache,
        _ => return None,
    };
    let loop_pc = match ops.get(after_call + 1) {
        Some(Op::Jump(target)) => *target as usize,
        _ => return None,
    };
    if after_call != expected_join + 4 {
        return None;
    }

    let v1 = d.jit_cache_preferred(*v1_cache)?;
    let scheduler_state = d.jit_cache_preferred(*scheduler_cache)?;
    let queue_method = d.jit_cache_preferred(*queue_method_cache)?;
    let blocks = d.jit_cache_preferred(*blocks_cache)?;
    let packet_id = d.jit_cache_preferred(*packet_id_cache)?;
    let queue_count = d.jit_cache_preferred(*queue_count_cache)?;
    let packet_link = d.jit_cache_preferred(*packet_link_store)?;
    let current_id = d.jit_cache_preferred(*current_id_cache)?;
    let check_method = d.jit_cache_preferred(*check_method_cache)?;
    let current = d.jit_cache_preferred(*current_cache)?;
    let target_queue = d.jit_cache_preferred(*target_queue_cache)?;
    let mark_method = d.jit_cache_preferred(*mark_method_cache)?;
    let state = d.jit_cache_preferred(*state_cache)?;
    let target_priority = d.jit_cache_preferred(*target_priority_cache)?;
    let current_priority = d.jit_cache_preferred(*current_priority_cache)?;
    let same_own = |owner: &Chunk, cache: u32, expected: crate::bytecode::IcState| {
        owner.jit_cache_preferred(cache).is_some_and(|actual| {
            actual.depth == 0
                && actual.recv_shape == expected.recv_shape
                && actual.slot == expected.slot
        })
    };
    let distinct_slots = |states: &[crate::bytecode::IcState]| {
        states.iter().enumerate().all(|(i, state)| {
            states[..i]
                .iter()
                .all(|earlier| earlier.slot != state.slot)
        })
    };
    if v1.depth != 0
        || v1.recv_shape != device_shape
        || v1.slot != parent_v1.slot
        || !same_own(d, *v1_store, v1)
        || scheduler_state.depth != 0
        || scheduler_state.recv_shape != device_shape
        || scheduler_state.slot == v1.slot
        || device_shape == tcb_shape
        || queue_method.depth != 1
        || queue_method.recv_shape != blocks.recv_shape
        || blocks.depth != 0
        || blocks.recv_shape == device_shape
        || blocks.recv_shape == tcb_shape
        || queue_count.depth != 0
        || queue_count.recv_shape != blocks.recv_shape
        || current_id.depth != 0
        || current_id.recv_shape != blocks.recv_shape
        || current.depth != 0
        || current.recv_shape != blocks.recv_shape
        || !same_own(chunk, outer_current_store, current)
        || packet_id.depth != 0
        || packet_link.depth != 0
        || packet_link.recv_shape != packet_id.recv_shape
        || !same_own(d, *packet_id_store, packet_id)
        || packet_id.recv_shape == device_shape
        || packet_id.recv_shape == tcb_shape
        || packet_id.recv_shape == blocks.recv_shape
        || check_method.depth != 1
        || check_method.recv_shape != target_queue.recv_shape
        || target_queue.depth != 0
        || target_queue.recv_shape != tcb_shape
        || !same_own(d, *target_queue_store, target_queue)
        || mark_method.depth != 1
        || mark_method.recv_shape != state.recv_shape
        || state.depth != 0
        || state.recv_shape != target_queue.recv_shape
        || !same_own(d, *state_store, state)
        || target_priority.depth != 0
        || target_priority.recv_shape != target_queue.recv_shape
        || current_priority.depth != 0
        || current_priority.recv_shape != tcb_shape
        || current_priority.slot != target_priority.slot
        || !distinct_slots(&[blocks, queue_count, current_id, current])
        || !distinct_slots(&[packet_id, packet_link])
        || !distinct_slots(&[target_queue, state, target_priority])
        || d.jit_name_number(*runnable_cache)
            .and_then(exact_i32_const)
            .is_none()
    {
        return None;
    }
    let queue_target = d.jit_inline_target(*queue_target);
    let check_target = d.jit_inline_target(*check_target);
    let mark_target = d.jit_inline_target(*mark_target);
    if queue_target.argc != 1
        || !queue_target.check_this
        || check_target.argc != 2
        || !check_target.check_this
        || mark_target.argc != 0
        || !mark_target.check_this
    {
        return None;
    }
    let expected = |target: &crate::bytecode::InlineTarget| {
        target.pin.upgrade().map(|o| {
            let some: Option<crate::value::Gc> = Some(o);
            unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
        })
    };
    Some(SchedulerDeviceQueuePlan {
        loop_pc,
        scheduler: scheduler_state,
        queue_method,
        queue_expected: expected(queue_target)?,
        blocks,
        packet_id,
        queue_count,
        packet_link,
        current_id,
        check_method,
        check_expected: expected(check_target)?,
        current,
        target_queue,
        mark_method,
        mark_expected: expected(mark_target)?,
        state,
        runnable_cache: d.jit_name_cache_ptr(*runnable_cache),
        target_priority,
        current_priority,
    })
}

/// Plan `while ((peek = next.link) != null) next = peek`.  The optimized region borrows linked
/// objects through the still-rooted initial list, performs no per-node RC operations, then
/// materializes the two locals exactly once at the exit or a guarded side exit.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_linked_scan(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<LinkedScanPlan> {
    use crate::bytecode::Op;
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || PACKED_LOCAL_SLOTS
        || !get_prop_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        return None;
    }
    let end = head.checked_add(9)?;
    let [
        Op::GetPropLocal(next, _, cache),
        Op::Dup,
        Op::StoreLocal(peek),
        Op::Const(null),
        cmp @ (Op::NotEq | Op::StrictNotEq),
        Op::JumpIfFalse(exit),
        Op::LoadLocal(peek_read),
        Op::StoreLocal(next_store),
        Op::Jump(back),
    ] = ops.get(head..end)?
    else {
        return None;
    };
    let loose_null_compare = matches!(cmp, Op::NotEq);
    if next != next_store
        || peek != peek_read
        || next == peek
        || *back as usize != head
        || *exit as usize <= end - 1
        || !chunk.jit_const_copyable(*null)
        || chunk.jit_const_bits(*null) != (2, 0)
    {
        return None;
    }
    let lp = cfg.loop_at_header(head)?;
    if lp.latches.len() != 1
        || cfg.blocks()[lp.latches[0].0 as usize].end != end
        || lp.blocks.iter().any(|id| {
            let block = &cfg.blocks()[id.0 as usize];
            block.start < head
                || block.end > end
                || block.stack_in != Some(0)
                || block.stack_out != Some(0)
        })
        || crate::jit_ir::RegionIr::build_loop(chunk, cfg, head).is_err()
    {
        return None;
    }
    let link = chunk.jit_cache_preferred(*cache)?;
    if link.depth != 0 {
        return None;
    }
    let next_off = *next as u32 * 16;
    let peek_off = *peek as u32 * 16;
    if next_off + 16 >= 4096 || peek_off + 16 >= 4096 {
        return None;
    }
    Some(LinkedScanPlan {
        exit_pc: *exit as usize,
        next_off,
        peek_off,
        link,
        loose_null_compare,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn exact_i32_const(bits: u64) -> Option<i64> {
    let value = f64::from_bits(bits);
    if !value.is_finite()
        || value == 0.0 && bits == (-0.0f64).to_bits()
        || value.fract() != 0.0
        || value < i32::MIN as f64
        || value > i32::MAX as f64
    {
        return None;
    }
    Some(value as i32 as i64)
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_numeric_diamond(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    cfg: &crate::jit_ir::Cfg,
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<NumericDiamondPlan> {
    use crate::bytecode::{Op, UpdKind};
    let end = head.checked_add(18)?;
    let body = ops.get(head..end)?;
    let [
        Op::LoadLocal(index),
        Op::LoadName(_, limit_cache),
        Op::Lt,
        Op::JumpIfFalse(exit),
        Op::LoadThis,
        Op::UpdateProp(counter_name, counter_update_cache, UpdKind::IncDiscard),
        Op::GetPropThis(counter_name_read, counter_read_cache),
        Op::Const(threshold),
        Op::Gt,
        Op::JumpIfFalse(no_reset),
        Op::Const(reset),
        Op::SetPropThisDrop(counter_name_set, counter_set_cache),
        Op::GetPropLocal(owner, _, array_cache),
        Op::LoadLocal(index_read),
        Op::GetPropThis(counter_name_store, counter_store_cache),
        Op::SetElemDrop,
        Op::UpdateLocal(index_update, UpdKind::IncDiscard),
        Op::Jump(back),
    ] = body
    else {
        return None;
    };
    macro_rules! reject {
        ($why:expr) => {{
            if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
                eprintln!("[jit-region-plan] head {head}: reject {}", $why);
            }
            return None;
        }};
    }
    if fast & (1 << 21) == 0
        || std::env::var_os("LUMEN_JIT_NO_CFG_REGION").is_some()
        || PACKED_LOCAL_SLOTS
    {
        reject!("disabled");
    }
    if !get_prop_inlinable(layout)
        || !set_prop_inlinable(layout)
        || !elem_inlinable(layout)
        || !packed_elem_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
    {
        reject!("layout");
    }
    if *back as usize != head
        || *no_reset as usize != head + 12
        || *exit as usize <= end - 1
        || index != index_read
        || index != index_update
        || counter_name != counter_name_read
        || counter_name != counter_name_set
        || counter_name != counter_name_store
    {
        reject!("shape");
    }

    let lp = cfg.loop_at_header(head)?;
    if lp.latches.len() != 1
        || cfg.blocks()[lp.latches[0].0 as usize].end != end
        || lp.blocks.iter().any(|id| {
            let block = &cfg.blocks()[id.0 as usize];
            block.start < head
                || block.end > end
                || block.stack_in != Some(0)
                || block.stack_out != Some(0)
        })
        || crate::jit_ir::RegionIr::build_loop(chunk, cfg, head).is_err()
    {
        reject!("cfg");
    }

    let Some(counter) = chunk.jit_cache_preferred(*counter_update_cache) else {
        reject!("counter cache empty/polymorphic");
    };
    let same_counter = |cache: u32| {
        chunk.jit_cache_preferred(cache).is_some_and(|state| {
            state.depth == 0
                && state.recv_shape == counter.recv_shape
                && state.slot == counter.slot
        })
    };
    if counter.depth != 0
        || !same_counter(*counter_read_cache)
        || !same_counter(*counter_set_cache)
        || !same_counter(*counter_store_cache)
    {
        reject!("counter cache");
    }
    let Some(array_prop) = chunk.jit_cache_preferred(*array_cache) else {
        reject!("array cache empty/polymorphic");
    };
    if array_prop.depth != 0 {
        reject!("array cache");
    }
    if chunk.jit_name_number(*limit_cache).is_none() {
        reject!("limit feedback");
    }
    let Some(threshold) = chunk
        .jit_const_num(*threshold)
        .and_then(exact_i32_const)
    else {
        reject!("threshold constant");
    };
    let Some(reset) = chunk.jit_const_num(*reset).and_then(exact_i32_const) else {
        reject!("reset constant");
    };
    let index_off = *index as u32 * 16;
    let owner_off = *owner as u32 * 16;
    if index_off + 16 >= 4096 || owner_off + 16 >= 4096 {
        reject!("slot range");
    }
    Some(NumericDiamondPlan {
        head,
        exit_pc: *exit as usize,
        index_off,
        owner_off,
        limit_cache: chunk.jit_name_cache_ptr(*limit_cache),
        counter,
        array_prop,
        threshold,
        reset,
    })
}

/// Try to recognize a *numeric register chain* starting at `start`: a maximal run of ops whose
/// intermediate values can live entirely in FP registers — locals, dense elements, float
/// arithmetic, cached names — ending either naturally or in a fused compare+branch. Every op
/// consumes only values produced *within* the chain (tracked by `vdepth`), so each value is a
/// proven Num in a register: arithmetic needs no tag checks at all and the compare+branch needs
/// no guards whatsoever. Returns the chain and how many bytecode ops it covers (`None` if
/// shorter than 3 ops — plain templates are fine for those).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn build_chain(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    start: usize,
    targeted: &[bool],
    layout: &crate::value::JitLayout,
    fast: u32,
) -> Option<(Vec<(ChainOp, usize)>, usize)> {
    use crate::bytecode::Op;
    let in_range = |s: u16| (s as u32) * 16 + 16 < 4096;
    let elem_ok = fast & 1024 != 0 && get_elem_inlinable(layout);
    let name_ok = fast & 8192 != 0 && load_name_inlinable(layout);
    let prop_ok = fast & 256 != 0
        && get_prop_inlinable(layout)
        && std::env::var_os("LUMEN_JIT_NO_PROP_CHAIN").is_none();
    let prop_store_ok = prop_ok
        && fast & 65536 != 0
        && set_prop_inlinable(layout)
        && std::env::var_os("LUMEN_JIT_NO_PROP_STORE_CHAIN").is_none();
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
            Op::LoadLocal(s) if in_range(*s) => (ChainOp::Load(*s as u32 * 16), 1, 0),
            Op::UpdateLocal(s, kind) if in_range(*s) => {
                let pushes = !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard);
                (ChainOp::Update(*s as u32 * 16, *kind), pushes as usize, 0)
            }
            Op::GetElemLocal(x) if elem_ok && in_range(*x) && vdepth >= 1 => {
                (ChainOp::GetElem(*x as u32 * 16), 1, 1)
            }
            Op::SetElemLocal(x) if elem_ok && in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 16, true), 1, 2)
            }
            Op::SetElemLocalDrop(x) if elem_ok && in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 16, false), 0, 2)
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
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr if vdepth >= 2 => {
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
                    (ChainOp::Store(*s as u32 * 16), 0, 1)
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
            Op::GetPropThis(_, c) if prop_ok => {
                let Some(st) = chunk.jit_cache_preferred(*c) else {
                    break;
                };
                if st.depth > 2 || (st.depth == 2 && st.mid_ok & 1 == 0) {
                    break;
                }
                (ChainOp::LoadProp(u32::MAX, st), 1, 0)
            }
            Op::GetPropLocal(s, _, c) if prop_ok && in_range(*s) => {
                let Some(st) = chunk.jit_cache_preferred(*c) else {
                    break;
                };
                if st.depth > 2 || (st.depth == 2 && st.mid_ok & 1 == 0) {
                    break;
                }
                (ChainOp::LoadProp(*s as u32 * 16, st), 1, 0)
            }
            Op::SetPropThisDrop(n, c)
                if prop_store_ok
                    && vdepth >= 1
                    && !chunk.jit_name(*n).as_bytes().first().is_some_and(|b| b.is_ascii_digit()) =>
            {
                let Some(st) = chunk.jit_cache_preferred(*c) else {
                    break;
                };
                if st.depth != 0 {
                    break;
                }
                (ChainOp::StoreProp(u32::MAX, st), 0, 1)
            }
            Op::SetPropLocalDrop(s, n, c)
                if prop_store_ok
                    && vdepth >= 1
                    && in_range(*s)
                    && !chunk.jit_name(*n).as_bytes().first().is_some_and(|b| b.is_ascii_digit()) =>
            {
                let Some(st) = chunk.jit_cache_preferred(*c) else {
                    break;
                };
                if st.depth != 0 {
                    break;
                }
                (ChainOp::StoreProp(*s as u32 * 16, st), 0, 1)
            }
            Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::EqEq
            | Op::NotEq
                if vdepth == 2 =>
            {
                match ops.get(pc + 1) {
                    Some(Op::JumpIfFalse(t)) if !targeted[pc + 1] => {
                        let neg = match ops[pc] {
                            Op::Lt => 5,                  // PL (unordered jumps)
                            Op::Gt => 13,                 // LE
                            Op::Le => 8,                  // HI
                            Op::Ge => 11,                 // LT
                            Op::StrictEq | Op::EqEq => 1, // NE
                            _ => 0,                       // EQ
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
        Some((
            ChainOp::ConstNum(_)
                | ChainOp::Load(_)
                | ChainOp::LoadName(_)
                | ChainOp::LoadProp(..),
            _
        ))
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
                ChainOp::ConstNum(_)
                | ChainOp::Load(_)
                | ChainOp::LoadName(_)
                | ChainOp::LoadProp(..) => (0, 1),
                ChainOp::StoreProp(..) => (1, 0),
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
                    ChainOp::ConstNum(_)
                        | ChainOp::Load(_)
                        | ChainOp::LoadName(_)
                        | ChainOp::LoadProp(..)
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
    // A speculative property producer is worthwhile when the chain actually computes with it.
    // Property-to-local transfer runs are common in generated code (EarleyBoyer in particular),
    // but they merely replace one compact property template with a larger guarded chain and can
    // repeatedly bail when the field is non-numeric. Leave those to the ordinary templates.
    let has_prop = chain
        .iter()
        .any(|(op, _)| matches!(op, ChainOp::LoadProp(..)));
    if has_prop {
        let useful = match chain.last() {
            Some((ChainOp::Bit(_), _)) => true,
            Some((ChainOp::StoreProp(..), _)) => chain
                .iter()
                .any(|(op, _)| matches!(op, ChainOp::Bit(_))),
            Some((ChainOp::CmpBranch(..), cmp_pc)) => match ops[*cmp_pc] {
                // Ordered comparisons necessarily request numeric coercion on the ordinary
                // path, so a numeric property guard is a productive speculation.
                crate::bytecode::Op::Lt
                | crate::bytecode::Op::Gt
                | crate::bytecode::Op::Le
                | crate::bytecode::Op::Ge => true,
                // Equality is frequently object/string identity in generated programs. Admit
                // only the global-constant form seen in numeric dispatch kernels; local/local
                // and property/property equality stay on the type-generic template.
                crate::bytecode::Op::StrictEq
                | crate::bytecode::Op::StrictNotEq
                | crate::bytecode::Op::EqEq
                | crate::bytecode::Op::NotEq => chain
                    .iter()
                    .any(|(op, _)| matches!(op, ChainOp::LoadName(_))),
                _ => false,
            },
            _ => false,
        };
        if !useful {
            return None;
        }
    }
    if std::env::var_os("LUMEN_JIT_PROP_CHAIN_LOG").is_some()
        && has_prop
    {
        let desc: Vec<String> = chain
            .iter()
            .map(|(_op, pc)| format!("{pc}:{:?}", ops[*pc]))
            .collect();
        eprintln!("[jit-prop-chain] {}", desc.join(" | "));
    }
    let consumed = chain.last().map_or(0, |&(op, p)| {
        p - start
            + if matches!(op, ChainOp::CmpBranch(..)) {
                2
            } else {
                1
            }
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_chain(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    chain: &[(ChainOp, usize)],
    pc_labels: &[usize],
    l_unwind: usize,
) {
    let mf = (layout.obj_props + layout.props_mirror_flags) as u32;
    let mirror = (layout.obj_props + layout.props_elems) as u32;
    let evp = (layout.dense_elems + layout.vec_ptr_off) as u32;
    let evl = (layout.dense_elems + layout.vec_len_off) as u32;
    let mvp = (layout.dense_mirror + layout.vec_ptr_off) as u32;
    let mvl = (layout.dense_mirror + layout.vec_len_off) as u32;
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let pr = layout.obj_proto as u32;
    let sh = (layout.obj_props + layout.props_shape) as u32;
    let el = (layout.obj_props + layout.props_elems) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let enl = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    let ev = layout.entry_value as i32;
    let num_ev = if layout.entry_accessor == layout.entry_value + 8 {
        ev
    } else {
        ev + 8
    };
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
    // Receiver cache: slot byte offset → registers holding validated receiver state. The chain
    // fast path calls no helpers, so between element ops nothing can change the slot's tag, the
    // object's exotic status, the ic-safe flag — or, in Mirror mode, the mirror's coherence,
    // length, or data pointer (the in-chain slim store only overwrites payloads; growth and
    // hole-creation bail). Mirror mode pins the whole element fast path in registers: `base` +
    // the mirror data pointer and length, validated MIRROR_OK|NO_HOLES once — so later reads
    // are a bounds check + one indexed load, the shape one dispatch loop hits 5-6 times per
    // iteration on the same one or two arrays (NavierStokes' lin_solve). Classic mode caches
    // only the base (register pressure, or the flags check failed at fill and the per-op
    // mirror/classic dance answers each access).
    // Cache registers live in x2-x8: `emit_name_ic_value_ptr` (in-chain LoadName) clobbers
    // x9-x17, so the caches survive it. Invalidation: an in-chain Store/Update to the receiver
    // slot drops its entry.
    enum RcMode {
        Classic,
        Mirror { mpreg: u32, mlreg: u32 },
    }
    struct RcEnt {
        off: u32,
        base: u32,
        mode: RcMode,
    }
    let mut rcache: Vec<RcEnt> = Vec::new();
    let mut rfree: Vec<u32> = vec![8, 7, 6, 5, 4, 3, 2];
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
            ChainOp::LoadProp(off, st) => {
                // Receiver-direct compact property probe. Unlike the ordinary property
                // template, a successful read never materializes a wide Value or adjusts a
                // refcount: the numeric payload enters the chain's FP register stack directly.
                if off == u32::MAX {
                    a.ldr_imm(14, 19, 48); // ctx.this_raw
                    a.ldrb_imm(9, 14, 0);
                    a.cmp_imm_w(9, 8);
                    a.b_cond(C_NE, guard!());
                    a.ldr_imm(10, 14, 8);
                } else {
                    a.ldrb_imm(9, 22, off);
                    a.cmp_imm_w(9, 8);
                    a.b_cond(C_NE, guard!());
                    a.ldr_imm(10, 22, off + 8);
                }
                a.add_imm(11, 10, rcv);
                a.ldrb_imm(14, 11, ex);
                a.cmp_imm_w(14, none_tag);
                a.b_cond(C_NE, guard!());
                a.ldrb_imm(14, 11, plain);
                a.cbz(14, false, guard!());
                a.ldr_w_imm(14, 11, sh);
                a.mov_imm64(16, st.recv_shape as u64);
                a.cmp_reg_w(14, 16);
                a.b_cond(C_NE, guard!());
                if st.depth >= 1 {
                    a.ldr_imm(17, 11, pr);
                    a.cbz(17, true, guard!());
                    a.add_imm(11, 17, rcv);
                    a.ldrb_imm(14, 11, ex);
                    a.cmp_imm_w(14, none_tag);
                    a.b_cond(C_NE, guard!());
                    a.ldrb_imm(14, 11, plain);
                    a.cbz(14, false, guard!());
                    a.ldr_w_imm(14, 11, sh);
                    let expected = if st.depth == 1 {
                        st.holder_shape
                    } else {
                        st.mid_shape
                    };
                    a.mov_imm64(16, expected as u64);
                    a.cmp_reg_w(14, 16);
                    a.b_cond(C_NE, guard!());
                }
                if st.depth == 2 {
                    a.ldr_imm(17, 11, pr);
                    a.cbz(17, true, guard!());
                    a.add_imm(11, 17, rcv);
                    a.ldrb_imm(14, 11, ex);
                    a.cmp_imm_w(14, none_tag);
                    a.b_cond(C_NE, guard!());
                    a.ldrb_imm(14, 11, plain);
                    a.cbz(14, false, guard!());
                    a.ldr_w_imm(14, 11, sh);
                    a.mov_imm64(16, st.holder_shape as u64);
                    a.cmp_reg_w(14, 16);
                    a.b_cond(C_NE, guard!());
                }
                a.ldr_imm(16, 11, enl);
                a.mov_imm64(13, st.slot as u64);
                a.cmp_reg_x(13, 16);
                a.b_cond(C_HS, guard!());
                a.ldr_imm(15, 11, en);
                a.mov_imm64(16, es);
                a.madd(15, 13, 16, 15);
                guard_prop_data(a, 9, 15, ea, guard!());
                let rd = free.pop().expect("chain reg underflow");
                if layout.entry_accessor == layout.entry_value + 8 {
                    // Packed Property value: all bit patterns outside the reserved tagged
                    // prefixes are Numbers (including the canonical NaN prefix). Object has a
                    // negative prefix and therefore needs its own rejection before the range.
                    a.ldur(13, 15, ev);
                    a.lsr_imm(9, 13, 48);
                    a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_EQ, guard!());
                    let is_num = a.new_label();
                    a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_LO, is_num);
                    a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_LS, guard!());
                    a.bind(is_num);
                    a.fmov_d_x(rd, 13);
                } else {
                    a.ldrb_imm(9, 15, ev as u32);
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_NE, guard!());
                    a.ldur_d(rd, 15, ev + 8);
                }
                vregs.push((rd, false));
            }
            ChainOp::StoreProp(off, st) => {
                let (dv, _) = vregs.pop().expect("chain vstack");
                if off == u32::MAX {
                    a.ldr_imm(14, 19, 48); // ctx.this_raw
                    a.ldrb_imm(9, 14, 0);
                    a.cmp_imm_w(9, 8);
                    a.b_cond(C_NE, guard!());
                    a.ldr_imm(10, 14, 8);
                } else {
                    a.ldrb_imm(9, 22, off);
                    a.cmp_imm_w(9, 8);
                    a.b_cond(C_NE, guard!());
                    a.ldr_imm(10, 22, off + 8);
                }
                a.add_imm(11, 10, rcv);
                a.ldrb_imm(14, 11, ex);
                a.cmp_imm_w(14, none_tag);
                a.b_cond(C_NE, guard!());
                a.ldrb_imm(14, 11, plain);
                a.cbz(14, false, guard!());
                a.ldr_w_imm(14, 11, sh);
                a.mov_imm64(16, st.recv_shape as u64);
                a.cmp_reg_w(14, 16);
                a.b_cond(C_NE, guard!());
                a.ldr_imm(16, 11, enl);
                a.mov_imm64(13, st.slot as u64);
                a.cmp_reg_x(13, 16);
                a.b_cond(C_HS, guard!());
                a.ldr_imm(15, 11, en);
                a.mov_imm64(16, es);
                a.madd(15, 13, 16, 15);
                guard_prop_data(a, 9, 15, ea, guard!());
                guard_prop_writable(a, 9, 15, ew, guard!());
                if layout.entry_accessor == layout.entry_value + 8 {
                    // Replacing a Number needs no ownership work. Reject every tagged old
                    // value before committing, then write the chain result as a packed f64.
                    a.ldur(13, 15, ev);
                    a.lsr_imm(9, 13, 48);
                    a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_EQ, guard!());
                    let old_num = a.new_label();
                    a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_LO, old_num);
                    a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
                    a.cmp_reg_x(9, 16);
                    a.b_cond(C_LS, guard!());
                    a.bind(old_num);
                    a.fmov_x_d(13, dv);
                    a.stur(13, 15, ev);
                } else {
                    a.ldrb_imm(9, 15, ev as u32);
                    a.cmp_imm_w(9, 4);
                    a.b_cond(C_NE, guard!());
                    a.stur_d(dv, 15, ev + 8);
                }
                free.push(dv);
            }
            ChainOp::Update(off, kind) => {
                if let Some(k) = rcache.iter().position(|c| c.off == off) {
                    let ent = rcache.remove(k);
                    rfree.push(ent.base);
                    if let RcMode::Mirror { mpreg, mlreg } = ent.mode {
                        rfree.push(mpreg);
                        rfree.push(mlreg);
                    }
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
                let cached = rcache.iter().position(|c| c.off == xoff);
                let mode: Option<(u32, u32)> = match cached {
                    Some(k) => {
                        let ent = &rcache[k];
                        a.mov(11, ent.base);
                        match ent.mode {
                            RcMode::Mirror { mpreg, mlreg } => Some((mpreg, mlreg)),
                            RcMode::Classic => None,
                        }
                    }
                    None => {
                        // First access to this receiver in the chain: validate once.
                        a.ldrb_imm(10, 22, xoff);
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
                        if rfree.len() >= 3 {
                            // Mirror mode: prove coherent + hole-free once, pin data ptr and
                            // length. A flags miss bails the chain (the plain templates run
                            // the rest) — only mirror-incoherent or holey arrays pay that.
                            let base = rfree.pop().unwrap();
                            let mpreg = rfree.pop().unwrap();
                            let mlreg = rfree.pop().unwrap();
                            a.mov(base, 11);
                            a.ldrb_imm(12, 11, mf);
                            let mask = asm::logical_imm_w(
                                (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
                            )
                            .unwrap();
                            a.logic_imm_w(0, 12, 12, mask);
                            a.cmp_imm_w(
                                12,
                                (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
                            );
                            a.b_cond(C_NE, guard!());
                            a.ldr_imm(12, 11, mirror);
                            a.cbz(12, true, guard!());
                            a.ldr_imm(mpreg, 12, mvp);
                            a.ldr_imm(mlreg, 12, mvl);
                            rcache.push(RcEnt {
                                off: xoff,
                                base,
                                mode: RcMode::Mirror { mpreg, mlreg },
                            });
                            Some((mpreg, mlreg))
                        } else {
                            if let Some(base) = rfree.pop() {
                                a.mov(base, 11);
                                rcache.push(RcEnt {
                                    off: xoff,
                                    base,
                                    mode: RcMode::Classic,
                                });
                            }
                            None
                        }
                    }
                };
                if let Some((mpreg, mlreg)) = mode {
                    // Mirror-pinned receiver: bounds against the register copy, then one
                    // indexed access. Stores also sync the canonical entry payload (readers
                    // outside the chain trust entries) and keep the ALL_I32 flag honest.
                    a.cmp_reg_x(9, mlreg);
                    a.b_cond(C_HS, guard!());
                    if !is_set {
                        a.ldr_d_lsl3(dk, mpreg, 9);
                        vregs.push((dk, false));
                    } else {
                        a.ldr_imm(12, 11, el);
                        a.cbz(12, true, guard!());
                        a.ldr_imm(12, 12, evp);
                        a.add_shifted(12, 12, 9, 2);
                        a.ldr_w_imm(13, 12, 0);
                        a.cmn_imm_w(13, 1);
                        a.b_cond(C_EQ, guard!()); // hole: property creation → plain path
                        a.ldr_imm(15, 11, en);
                        a.movz(14, es as u32, 0);
                        a.madd(15, 13, 14, 15);
                        a.stur_d(dv, 15, num_ev); // MIRROR_OK ⇒ plain writable data Num
                        a.str_d_lsl3(dv, mpreg, 9);
                        // Flag-first ALL_I32 upkeep (dv int-ness is unknown in this tier).
                        let i32_done = a.new_label();
                        a.ldrb_imm(13, 11, mf);
                        let i32_bit =
                            asm::logical_imm_w(crate::value::MIRROR_ALL_I32 as u32).unwrap();
                        a.logic_imm_w(0, 12, 13, i32_bit);
                        a.cbz(12, false, i32_done);
                        a.fcvtzs_w_d(12, dv);
                        a.scvtf_d_w(1, 12);
                        a.fmov_x_d(12, 1);
                        a.fmov_x_d(14, dv);
                        a.cmp_reg_x(12, 14);
                        a.b_cond(C_EQ, i32_done);
                        let clear =
                            asm::logical_imm_w(!(crate::value::MIRROR_ALL_I32 as u32)).unwrap();
                        a.logic_imm_w(0, 13, 13, clear);
                        a.strb_imm(13, 11, mf);
                        a.bind(i32_done);
                        free.push(dk);
                        if keep {
                            vregs.push((dv, viv));
                        } else {
                            free.push(dv);
                        }
                    }
                    if used > 0 {
                        bails.push((idx, bail, pre_op));
                    }
                    continue;
                }
                let mirror_done = a.new_label();
                let classic = a.new_label();
                if !is_set {
                    // Mirror read: coherent + hole-free ⇒ bounds + one indexed load, value
                    // known Num. Any miss (flags, range) answers classically below.
                    a.ldrb_imm(12, 11, mf);
                    let mask = asm::logical_imm_w(
                        (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
                    )
                    .unwrap();
                    a.logic_imm_w(0, 12, 12, mask);
                    a.cmp_imm_w(
                        12,
                        (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32,
                    );
                    a.b_cond(C_NE, classic);
                    a.ldr_imm(12, 11, mirror);
                    a.cbz(12, true, classic);
                    a.ldr_imm(14, 12, mvl);
                    a.cmp_reg_x(9, 14);
                    a.b_cond(C_HS, classic);
                    a.ldr_imm(12, 12, mvp);
                    a.ldr_d_lsl3(dk, 12, 9);
                    a.b(mirror_done);
                } else {
                    // Mirror-slim store: MIRROR_OK proves every (non-hole) element is a plain
                    // writable data Num, so the accessor/writable/old-value dance collapses to
                    // a payload overwrite in the entry plus the mirror word. A hole (elems
                    // NO_SLOT) would CREATE a property — classic handles it.
                    a.ldrb_imm(12, 11, mf);
                    let ok_bit = asm::logical_imm_w(crate::value::MIRROR_OK as u32).unwrap();
                    a.logic_imm_w(0, 12, 12, ok_bit);
                    a.cbz(12, false, classic);
                    a.ldr_imm(12, 11, mirror);
                    a.cbz(12, true, classic);
                    a.ldr_imm(14, 12, mvl);
                    a.cmp_reg_x(9, 14);
                    a.b_cond(C_HS, classic);
                    a.ldr_imm(12, 11, el);
                    a.cbz(12, true, classic);
                    a.ldr_imm(12, 12, evp);
                    a.add_shifted(12, 12, 9, 2);
                    a.ldr_w_imm(13, 12, 0);
                    a.cmn_imm_w(13, 1);
                    a.b_cond(C_EQ, classic);
                    a.ldr_imm(15, 11, en);
                    a.movz(14, es as u32, 0);
                    a.madd(15, 13, 14, 15);
                    a.stur_d(dv, 15, num_ev);
                    a.ldr_imm(12, 11, mirror);
                    a.ldr_imm(12, 12, mvp);
                    a.str_d_lsl3(dv, 12, 9);
                    // Flag-first ALL_I32 upkeep (dv int-ness is unknown in this tier).
                    let i32_done = a.new_label();
                    a.ldrb_imm(13, 11, mf);
                    let i32_bit = asm::logical_imm_w(crate::value::MIRROR_ALL_I32 as u32).unwrap();
                    a.logic_imm_w(0, 12, 13, i32_bit);
                    a.cbz(12, false, i32_done);
                    a.fcvtzs_w_d(12, dv);
                    a.scvtf_d_w(1, 12);
                    a.fmov_x_d(12, 1);
                    a.fmov_x_d(14, dv);
                    a.cmp_reg_x(12, 14);
                    a.b_cond(C_EQ, i32_done);
                    let clear = asm::logical_imm_w(!(crate::value::MIRROR_ALL_I32 as u32)).unwrap();
                    a.logic_imm_w(0, 13, 13, clear);
                    a.strb_imm(13, 11, mf);
                    a.bind(i32_done);
                    a.b(mirror_done);
                }
                a.bind(classic);
                if layout.entry_accessor == layout.entry_value + 8 {
                    a.b(guard!());
                }
                a.ldr_imm(12, 11, el);
                a.cbz(12, true, guard!());
                a.ldr_imm(14, 12, evl);
                a.cmp_reg_x(9, 14);
                a.b_cond(C_HS, guard!());
                a.ldr_imm(12, 12, evp);
                a.add_shifted(12, 12, 9, 2);
                a.ldr_w_imm(13, 12, 0);
                a.cmn_imm_w(13, 1);
                a.b_cond(C_EQ, guard!());
                a.ldr_imm(15, 11, en);
                a.movz(9, es as u32, 0); // entry stride (< 65536; the key index in x9 is dead)
                a.madd(15, 13, 9, 15);
                guard_prop_data(a, 9, 15, ea, guard!());
                if is_set {
                    guard_prop_writable(a, 9, 15, ew, guard!());
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
                    // commit: entry = Num(dv); drop the old value
                    a.movz(9, 4, 0);
                    a.stur(9, 15, ev);
                    a.stur_d(dv, 15, ev + 8);
                    let no_dec = a.new_label();
                    a.cmp_imm_w(14, 6);
                    a.b_cond(C_LO, no_dec);
                    a.ldur(13, 12, strong);
                    a.sub_imm(13, 13, 1);
                    a.stur(13, 12, strong);
                    a.bind(no_dec);
                    // Element mirror: dv is a proven Num; int-ness is unknown in this tier.
                    emit_mirror_store(
                        a,
                        layout,
                        11,
                        MirrorKey::F64InDreg(dk),
                        MirrorVal::Num(dv, false),
                    );
                    a.bind(mirror_done);
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
                    a.bind(mirror_done);
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
                if let Some(k) = rcache.iter().position(|c| c.off == off) {
                    let ent = rcache.remove(k);
                    rfree.push(ent.base);
                    if let RcMode::Mirror { mpreg, mlreg } = ent.mode {
                        rfree.push(mpreg);
                        rfree.push(mlreg);
                    }
                }
                let (dv, _) = vregs.pop().expect("chain vstack");
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
                // The validator clobbers x9-x17 only — receiver caches (x2-x8) survive it.
                // Shared cache validation (scope or global mode) leaves x14 → the Value.
                emit_name_ic_value_ptr(a, layout, cache_ptr, guard!(), true);
                let rd = free.pop().expect("chain reg underflow");
                let loaded = a.new_label();
                if layout.entry_accessor == layout.entry_value + 8 {
                    let wide = a.new_label();
                    a.cbz(7, false, wide);
                    a.ldur(9, 14, 0);
                    a.lsr_imm(10, 9, 48);
                    let number = a.new_label();
                    a.movz(11, (crate::value::PACK_OBJ >> 48) as u32, 0);
                    a.cmp_reg_x(10, 11);
                    a.b_cond(C_EQ, guard!());
                    a.movz(11, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
                    a.cmp_reg_x(10, 11);
                    a.b_cond(C_LO, number);
                    a.movz(11, (crate::value::PACK_SYM >> 48) as u32, 0);
                    a.cmp_reg_x(10, 11);
                    a.b_cond(C_LS, guard!());
                    a.bind(number);
                    a.fmov_d_x(rd, 9);
                    a.b(loaded);
                    a.bind(wide);
                }
                a.ldurb(9, 14, 0);
                a.cmp_imm_w(9, 4);
                a.b_cond(C_NE, guard!()); // only a Num can live in a register
                a.ldur_d(rd, 14, 8);
                a.bind(loaded);
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
        a.add_imm(20, 20, 16);
    }
    a.b(done);
    // ---- bail paths: spill the pre-op virtual stack, then re-run the rest via the helper ----
    for (idx, label, snap) in bails {
        a.bind(label);
        for &(r, _) in &snap {
            a.movz(9, 4, 0);
            a.stur(9, 20, 0);
            a.stur_d(r, 20, 8);
            a.add_imm(20, 20, 16);
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

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
/// What a chain op pushes, precomputed by the planner (see the module comment above).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PushKind {
    None,
    K(u64),
    I { neg: bool },
    D { iv: bool },
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
    /// Receiver slots validated once. In mirror mode the cached register holds the raw-f64
    /// element buffer's data pointer (`Props::mirror`) with the length in `len_reg`, and
    /// element reads are one indexed load; classic mode caches the object base and walks
    /// entries per access.
    receivers: Vec<ReceiverPlan>,
    /// GetElem chain idx → pin register holding its (guarded) result for later reuse.
    elem_retain: Vec<(usize, u32)>,
    /// GetElem chain idx → the retaining chain idx whose pin it copies from.
    elem_reuse: Vec<(usize, usize)>,
    /// Bit (chain idx, operand side) → pin register: retain the guarded ToInt32 result / reuse.
    conv_retain: Vec<((usize, u8), u32)>,
    conv_reuse: Vec<((usize, u8), u32)>,
    /// Per SetElem chain idx: the stored value is a proven exact-i32 (mirror flag upkeep).
    setelem_i32: crate::fasthash::FastMap<usize, bool>,
    /// Cached free names read in the region, pinned once in the preamble. The region is
    /// helper-free and its op vocabulary writes only locals and elements, so a name binding
    /// cannot change while the loop spins: one validation covers every iteration (loop bounds
    /// are typically closure vars — `i < width` — and paid a full name-IC probe per iteration
    /// as plain chains).
    names: Vec<NamePlan>,
    /// Some pin drew from x23-x28 (callee-saved): the loop brackets itself with save/restore
    /// pairs — a preamble spill, and a reload on every exit and bail path.
    uses_ext: bool,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct NamePlan {
    /// The `NameIc` cell address (`Chunk::jit_name_cache_ptr`).
    ptr: usize,
    /// f64 home in a d-register (allocated from the same d8..d15 bank as `SlotRes::F`).
    dreg: u32,
    /// Preamble adds the one-time exact-int proof (the name feeds a key or bit op).
    int_checked: bool,
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
/// How a loop-chain receiver is cached (see `LoopPlan::receivers`).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct ReceiverPlan {
    off: u32,
    /// x16/x17 (receivers 3-4 draw from the pin pool): the validated object base.
    reg: u32,
    /// Element accesses go through the raw-f64 mirror (preamble-proven coherent and hole-free;
    /// the buffer pointer/length load per access — same cache line, far cheaper than the
    /// entry chase they replace).
    mirror: bool,
    /// Any int-typed element read flows from this receiver (preamble then requires
    /// `MIRROR_ALL_I32`, letting those reads use a bare fcvtzs).
    int_reads: bool,
    /// Leftover pin registers holding the mirror length / mirror data / elems data / entries
    /// data pointers (all stable in-region: the vocabulary is helper-free and slim stores
    /// never grow or reallocate). Each pin shaves a dependent load off every element access —
    /// the hot lin_solve-shape loop hits one receiver 5-6 times per iteration.
    mlreg: Option<u32>,
    mpreg: Option<u32>,
    elpreg: Option<u32>,
    enreg: Option<u32>,
}

/// Integer-range bookkeeping for iv decisions: |v| ≤ 2^exp and integral. 255 = unknown/not
/// integral. Kept crude on purpose — it only has to prove products/sums of masked values stay
/// under 2^62.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy)]
struct NumInfo {
    integral: bool,
    exp: u32,
    neg: bool,
}
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl NumInfo {
    fn unknown() -> NumInfo {
        NumInfo {
            integral: false,
            exp: 255,
            neg: true,
        }
    }
    fn iv(&self) -> bool {
        self.integral && self.exp <= 62
    }
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn plan_loop(
    chunk: &Chunk,
    ops: &[crate::bytecode::Op],
    head: usize,
    targeted: &[bool],
    layout: &crate::value::JitLayout,
    fast: u32,
    cfg: &crate::jit_ir::Cfg,
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
    let in_range = |s: u16| (s as u32) * 16 + 16 < 4096;
    let name_ok = fast & 8192 != 0 && load_name_inlinable(layout);

    // ---- region discovery: the shared CFG admits only the old emitter's linear-loop shape.
    // Forward diamonds are intentionally left for the SSA/fixed-home region lowering.
    let jump_pc = cfg.linear_loop_latch(ops, head)?;
    if jump_pc == head + 1 {
        reject!("empty region");
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
            Op::LoadLocal(s) if in_range(*s) => (ChainOp::Load(*s as u32 * 16), 1, 0),
            Op::UpdateLocal(s, kind) if in_range(*s) => {
                let pushes = !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard);
                (ChainOp::Update(*s as u32 * 16, *kind), pushes as usize, 0)
            }
            Op::GetElemLocal(x) if in_range(*x) && vdepth >= 1 => {
                (ChainOp::GetElem(*x as u32 * 16), 1, 1)
            }
            Op::SetElemLocal(x) if in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 16, true), 1, 2)
            }
            Op::SetElemLocalDrop(x) if in_range(*x) && vdepth >= 2 => {
                (ChainOp::SetElem(*x as u32 * 16, false), 0, 2)
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
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr if vdepth >= 2 => {
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
                (ChainOp::Store(*s as u32 * 16), 0, 1)
            }
            Op::Pop if vdepth >= 1 => (ChainOp::Pop, 0, 1),
            Op::Dup if vdepth >= 1 => (ChainOp::Dup, 1, 0),
            Op::ToPropKeyLocal(_) if vdepth >= 1 => (ChainOp::KeyNop, 0, 0),
            Op::LoadName(_, c) if name_ok => {
                (ChainOp::LoadName(chunk.jit_name_cache_ptr(*c)), 1, 0)
            }
            Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::EqEq
            | Op::NotEq
                if vdepth == 2 =>
            {
                match ops.get(pc + 1) {
                    Some(Op::JumpIfFalse(t)) if (*t as usize) > jump_pc => {
                        if exit_pc.is_some() {
                            return None; // one exit only
                        }
                        let neg = match ops[pc] {
                            Op::Lt => 5,                  // PL (unordered jumps)
                            Op::Gt => 13,                 // LE
                            Op::Le => 8,                  // HI
                            Op::Ge => 11,                 // LT
                            Op::StrictEq | Op::EqEq => 1, // NE
                            _ => 0,                       // EQ
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
    // Free names are loop-invariant (nothing in the vocabulary writes a binding): every read
    // of one cache ptr is the same node.
    let mut name_src: crate::fasthash::FastMap<usize, usize> = Default::default();
    let mut names_order: Vec<usize> = Vec::new();
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
            ChainOp::LoadName(ptr) => {
                let node = *name_src.entry(ptr).or_insert(idx);
                if !names_order.contains(&ptr) {
                    names_order.push(ptr);
                }
                stack.push(node);
            }
            ChainOp::LoadProp(..) | ChainOp::StoreProp(..) => {
                unreachable!("loop discovery never admits property operations")
            }
        }
    }

    // Elem ops present require the inline layout; receivers must never be written in-region.
    if !elem_nodes.is_empty() || !receivers.is_empty() {
        if fast & 1024 == 0 || !get_elem_inlinable(layout) {
            reject!("elem layout");
        }
    }
    if receivers.len() > 4 {
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

    // Names feeding int contexts get the one-time exact-int preamble proof (like int_checked
    // slots), so integer consumers take them with a bare fcvtzs.
    let int_checked_names: Vec<usize> = names_order
        .iter()
        .copied()
        .filter(|p| name_src.get(p).is_some_and(|&nd| needs_int[nd]))
        .collect();

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
                ChainOp::LoadProp(..) | ChainOp::StoreProp(..) => {
                    unreachable!("loop discovery never admits property operations")
                }
            };
            let mut popped: Vec<usize> = Vec::new();
            for _ in 0..pops {
                popped.push(stack2.pop().expect("residency stack"));
            }
            match *cop {
                ChainOp::Load(off) => {
                    let nd = bind2
                        .get(&off)
                        .copied()
                        .unwrap_or_else(|| *src2.entry(off).or_insert(idx));
                    stack2.push(nd);
                }
                ChainOp::Update(off, kind) => {
                    let cur = bind2.get(&off).copied().or(src2.get(&off).copied());
                    src2.entry(off).or_insert(idx);
                    bind2.insert(off, idx);
                    if pushes == 1 {
                        match kind {
                            UpdKind::PostInc | UpdKind::PostDec => stack2.push(cur.unwrap_or(idx)),
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
    // Per SetElem chain idx: stored value proven exact-i32 (final round).
    let mut setelem_i32: crate::fasthash::FastMap<usize, bool> = Default::default();
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
            if last
                .iter()
                .any(|&(r, kn, wi)| r == rcv && kn == key && wi == elem_int[k])
            {
                if elem_int[k] {
                    dups += 1;
                }
            } else {
                last.push((rcv, key, elem_int[k]));
            }
        }
        dups.min(1)
    };
    let mut pins_demoted = 0usize;
    let pins_wanted = want_pin;
    'budget: loop {
        widened.clear();
        stable = false;
        for _round in 0..64 {
            plan_kinds = vec![PushKind::None; n];
            bit_kinds.clear();
            setelem_i32.clear();
            // (kind, info) per virtual value; slot state per off.
            let mut vstack: Vec<(PushKind, NumInfo)> = Vec::new();
            let mut slot_iv: crate::fasthash::FastMap<u32, NumInfo> = Default::default();
            for &off in &int_checked {
                slot_iv.insert(
                    off,
                    NumInfo {
                        integral: true,
                        exp: 31,
                        neg: true,
                    },
                );
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
                        push!(
                            PushKind::K(bits),
                            NumInfo {
                                integral,
                                exp,
                                neg: f < 0.0
                            }
                        );
                    }
                    ChainOp::Load(off) => {
                        if i_slots.contains(&off) {
                            let exp = slot_exp.get(&off).copied().unwrap_or(31);
                            push!(
                                PushKind::I { neg: true },
                                NumInfo {
                                    integral: true,
                                    exp,
                                    neg: true
                                }
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
                                    NumInfo {
                                        integral: true,
                                        exp: 31,
                                        neg: true
                                    }
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
                                NumInfo {
                                    integral: true,
                                    exp: 31,
                                    neg: true
                                }
                            );
                        } else {
                            push!(PushKind::D { iv: false }, NumInfo::unknown());
                        }
                    }
                    ChainOp::SetElem(_, keep) => {
                        let (vk, vinf) = pop!();
                        pop!();
                        // Exact-i32 proof for the mirror: an int-kind value bounded to i32 (int
                        // kinds can never carry -0.0).
                        setelem_i32.insert(idx, matches!(vk, PushKind::I { .. }) && vinf.exp <= 31);
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
                            // -0.0 is "integral" but has no integer representation: its sign would
                            // erase through int arithmetic.
                            PushKind::K(b) => {
                                inf.integral && inf.exp <= 52 && b != (-0.0f64).to_bits()
                            }
                            // Proven-integral f64 (entry-checked preload or tracked store): a bare
                            // fcvtzs is exact (the entry guards reject -0.0).
                            PushKind::D { .. } => inf.integral && inf.exp <= 52,
                            _ => false,
                        };
                        if f != 3 && exp <= 52 && int_side(ak, ainf) && int_side(bk, binf) {
                            let neg = ainf.neg || binf.neg || f == 1;
                            push!(
                                PushKind::I { neg },
                                NumInfo {
                                    integral: true,
                                    exp,
                                    neg
                                }
                            );
                        } else {
                            let inf = NumInfo {
                                integral: integral && exp <= 62,
                                exp,
                                neg: true,
                            };
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
                                    None => NumInfo {
                                        integral: true,
                                        exp: 32,
                                        neg: true,
                                    },
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
                                    None => NumInfo {
                                        integral: true,
                                        exp: 32,
                                        neg: true,
                                    },
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
                                    _ => NumInfo {
                                        integral: true,
                                        exp: 32,
                                        neg: true,
                                    },
                                }
                            }
                            4 => NumInfo {
                                integral: true,
                                exp: 32,
                                neg: false,
                            },
                            _ => NumInfo {
                                integral: true,
                                exp: 32,
                                neg: true,
                            },
                        };
                        push!(PushKind::I { neg: inf.neg }, inf);
                    }
                    ChainOp::Neg => {
                        let (_, vinf) = pop!();
                        let inf = NumInfo {
                            integral: vinf.integral,
                            exp: vinf.exp,
                            neg: true,
                        };
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
                    ChainOp::LoadName(ptr) => {
                        let inf = if int_checked_names.contains(&ptr) {
                            NumInfo {
                                integral: true,
                                exp: 31,
                                neg: true,
                            }
                        } else {
                            NumInfo::unknown()
                        };
                        push!(PushKind::D { iv: inf.iv() }, inf);
                    }
                    ChainOp::LoadProp(..) | ChainOp::StoreProp(..) => {
                        unreachable!("loop discovery never admits property operations")
                    }
                }
                // Operand registers are freed only at op end, so an op needs its start-of-op
                // live set plus everything it pushes, simultaneously.
                i_peak = i_peak.max(i_live).max(i_start + i_pushed);
                d_peak = d_peak.max(d_live).max(d_start + d_pushed);
            }
            match demote {
                Some(off) => {
                    if std::env::var_os("LUMEN_JIT_LOOPLOG").is_some() {
                        eprintln!("[jit-loop] head {head}: demote I slot {}", off / 16);
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
        // Register budget: demote the least-used I resident and re-simulate when over; also
        // give up (a bounded number of) residents so the receiver/memo pins fit — a length
        // pin turns every element access into one load, worth far more than a counter's home.
        let over = i_peak + i_slots.len() > I_UNIVERSE.len();
        let pin_squeeze = pins_demoted < pins_wanted
            && i_peak + i_slots.len() + (pins_wanted - pins_demoted) > I_UNIVERSE.len();
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
                            v / 16,
                            if over { "pressure" } else { "pin" }
                        );
                    }
                    if !over {
                        pins_demoted += 1;
                    }
                    i_slots.retain(|&o| o != v);
                    slot_exp_head.remove(&v);
                    continue 'budget;
                }
                None if over => reject!(format!("i pressure: peak {i_peak}")),
                None => break,
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
    if f_slots.len() + names_order.len() > 8 {
        reject!(format!(
            "f pressure: {} slots + {} names",
            f_slots.len(),
            names_order.len()
        ));
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
    // Name homes come from the same d8..d15 bank, after the slot homes.
    let names: Vec<NamePlan> = names_order
        .iter()
        .map(|&ptr| {
            let dreg = next_d;
            next_d += 1;
            NamePlan {
                ptr,
                dreg,
                int_checked: int_checked_names.contains(&ptr),
            }
        })
        .collect();
    // Sanity: kinds recorded for the final residency sets. The last sim round used exactly
    // `i_slots`/all-resident F, matching the assignment above.

    // Which receivers feed int-typed element reads (their mirror mode also needs ALL_I32).
    let mut rcv_int: Vec<u32> = Vec::new();
    {
        let mut seen = 0usize;
        for (cop, _) in &chain {
            if let ChainOp::GetElem(off) = *cop {
                if elem_int[seen] && !rcv_int.contains(&off) {
                    rcv_int.push(off);
                }
                seen += 1;
            }
        }
    }

    // ---- memoization: duplicate element reads and repeated guarded ToInt32 conversions.
    // Node ids are SSA-like (an id never changes value), so a second element read with the same
    // (receiver, key id) — with no intervening element write — and a second Bit-op use of the
    // same unproven-f64 id can reuse the first result from a pinned register. Pins live in the
    // leftover resident registers; memos are dropped when none are free.
    // x pins: whatever the universe leaves after I residents and the transient reserve; d pins
    // from the resident bank's leftovers (d transients live in d16.. and never collide).
    // Pin pool: the caller-saved leftovers, then x23-x28 (callee-saved — using any obliges
    // the loop to bracket itself with save/restore pairs, a fixed ~6-instruction cost per
    // loop ENTRY against one shaved load per element access per iteration). Pops take the
    // caller-saved ones first.
    let mut free_pin_x: Vec<u32> = [28u32, 27, 26, 25, 24, 23]
        .into_iter()
        .chain(
            I_UNIVERSE
                .iter()
                .copied()
                .filter(|x| !slots.iter().any(|s| s.res == SlotRes::I(*x)))
                .skip(i_peak),
        )
        .collect();
    let mut free_pin_d: Vec<u32> = (next_d..16).collect();
    // Receivers 1-2 take x16/x17; 3-4 draw from the pin pool BEFORE the memo pins (a receiver
    // base is worth more than a memo — it carries every element access on that array). What
    // the pool still has after that pins the per-receiver vector fields, most-used first:
    // mirror length and data (every access), then elems/entries data (stores only).
    let mut rplans: Vec<ReceiverPlan> = Vec::new();
    let written: Vec<u32> = elem_writes.iter().map(|&(_, off)| off).collect::<Vec<_>>();
    for (k, &off) in receivers.iter().enumerate() {
        let reg = if k < 2 {
            16 + k as u32
        } else {
            match free_pin_x.pop() {
                Some(r) => r,
                None => reject!("too many receivers for the pin pool"),
            }
        };
        rplans.push(ReceiverPlan {
            off,
            reg,
            mirror: fast & 262144 != 0,
            int_reads: rcv_int.contains(&off),
            mlreg: None,
            mpreg: None,
            elpreg: None,
            enreg: None,
        });
    }
    if fast & 262144 != 0 {
        // Heaviest-accessed receiver first, and its FULL pin set before the next receiver
        // gets any: in the lin_solve shape one array carries 5 of 6 accesses per iteration —
        // splitting pins evenly left its mirror data pointer reloading every access.
        let weight = |off: u32| {
            elem_reads.iter().filter(|&&(_, r, _)| r == off).count()
                + elem_writes.iter().filter(|&&(_, r)| r == off).count()
        };
        let mut order: Vec<usize> = (0..rplans.len()).collect();
        order.sort_by_key(|&k| std::cmp::Reverse(weight(rplans[k].off)));
        for k in order {
            let rp = &mut rplans[k];
            rp.mlreg = free_pin_x.pop();
            rp.mpreg = free_pin_x.pop();
            if written.contains(&rp.off) {
                rp.elpreg = free_pin_x.pop();
                rp.enreg = free_pin_x.pop();
            }
        }
    }
    let receivers = rplans;
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
                let pin = if elem_int[k] {
                    free_pin_x.pop()
                } else {
                    free_pin_d.pop()
                };
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
        let vec_pins: usize = receivers
            .iter()
            .map(|r| {
                [r.mlreg, r.mpreg, r.elpreg, r.enreg]
                    .iter()
                    .filter(|p| p.is_some())
                    .count()
            })
            .sum();
        eprintln!(
            "[jit-loop] head {head}: CHAINED {} ops, {} slots ({} I), {} receivers ({} vec pins), {} names, memo elem {}r/{}u conv {}r/{}u",
            chain.len(),
            slots.len(),
            slots
                .iter()
                .filter(|s| matches!(s.res, SlotRes::I(_)))
                .count(),
            receivers.len(),
            vec_pins,
            names.len(),
            elem_retain.len(),
            elem_reuse.len(),
            conv_retain.len(),
            conv_reuse.len()
        );
    }
    let uses_ext = receivers.iter().any(|r| {
        r.reg >= 23
            || [r.mlreg, r.mpreg, r.elpreg, r.enreg]
                .iter()
                .flatten()
                .any(|&x| x >= 23)
    }) || elem_retain.iter().any(|&(_, p)| p >= 23)
        || conv_retain.iter().any(|&(_, p)| p >= 23);
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
        setelem_i32,
        names,
        uses_ext,
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
/// A virtual value during loop-chain emission.
#[derive(Clone, Copy)]
enum LV {
    K(u64),
    I(u32, bool), // x-register, may-be-negative
    D(u32, bool), // d-register, integral-valued
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_own_entry(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    rc_reg: u32,
    body_reg: u32,
    entry_reg: u32,
    state: crate::bytecode::IcState,
    writable: bool,
    fail: usize,
) {
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let plain = layout.obj_ic_plain as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;
    let entries = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let entries_len = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    a.add_imm(body_reg, rc_reg, rcv);
    a.ldrb_imm(9, body_reg, ex);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, body_reg, plain);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, body_reg, shape);
    a.mov_imm64(16, state.recv_shape as u64);
    a.cmp_reg_w(9, 16);
    a.b_cond(C_NE, fail);
    a.ldr_imm(16, body_reg, entries_len);
    a.mov_imm64(13, state.slot as u64);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, fail);
    a.ldr_imm(entry_reg, body_reg, entries);
    a.mov_imm64(16, layout.entry_size as u64);
    a.madd(entry_reg, 13, 16, entry_reg);
    guard_prop_data(a, 9, entry_reg, layout.entry_accessor as u32, fail);
    if writable {
        guard_prop_writable(a, 9, entry_reg, layout.entry_writable as u32, fail);
    }
}

/// Locate an own property entry after the receiver's ordinary-object kind and exact shape were
/// already proved by the enclosing scheduler role dispatch. The entries vector cannot move while
/// that dispatch remains direct: every generated task arm only updates existing property values
/// and any path capable of running user code leaves the role region first.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_own_entry_trusted_shape(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    rc_reg: u32,
    body_reg: u32,
    entry_reg: u32,
    state: crate::bytecode::IcState,
    writable: bool,
    fail: usize,
) {
    let entries = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let entries_len = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    a.add_imm(body_reg, rc_reg, layout.obj_from_rc as u32);
    a.ldr_imm(16, body_reg, entries_len);
    a.mov_imm64(13, state.slot as u64);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, fail);
    a.ldr_imm(entry_reg, body_reg, entries);
    a.mov_imm64(16, layout.entry_size as u64);
    a.madd(entry_reg, 13, 16, entry_reg);
    guard_prop_data(a, 9, entry_reg, layout.entry_accessor as u32, fail);
    if writable {
        guard_prop_writable(a, 9, entry_reg, layout.entry_writable as u32, fail);
    }
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_packed_number(
    a: &mut asm::Asm,
    entry_reg: u32,
    value_off: i32,
    dreg: u32,
    fail: usize,
) {
    a.ldur(13, entry_reg, value_off);
    a.lsr_imm(9, 13, 48);
    a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_EQ, fail);
    let number = a.new_label();
    a.movz(16, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_LO, number);
    a.movz(16, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_LS, fail);
    a.bind(number);
    a.fmov_d_x(dreg, 13);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_exact_i32(a: &mut asm::Asm, dreg: u32, out: u32, fail: usize) {
    a.fcvtzs_w_d(out, dreg);
    a.scvtf_d_w(1, out);
    a.fmov_x_d(13, 1);
    a.fmov_x_d(14, dreg);
    a.cmp_reg_x(13, 14); // also rejects -0.0, NaN, infinities, and out-of-range values
    a.b_cond(C_NE, fail);
    a.sxtw(out, out);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_packed_scalar(
    a: &mut asm::Asm,
    entry: u32,
    value_off: i32,
    out: u32,
    fail: usize,
) {
    let scalar = a.new_label();
    a.ldur(out, entry, value_off);
    a.lsr_imm(9, out, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_EQ, fail);
    a.movz(10, (crate::value::PACK_BIGINT >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_LO, scalar);
    a.movz(10, (crate::value::PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_LS, fail);
    a.bind(scalar);
}

/// Guard an exact method stored on the already-pinned immediate prototype `proto`. The receiver
/// shape/prototype identity is checked separately for every scheduler iteration.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_proto_method(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    proto: u32,
    body: u32,
    entry: u32,
    state: crate::bytecode::IcState,
    expected: usize,
    fail: usize,
) {
    let rcv = layout.obj_from_rc as u32;
    let entries = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let entries_len = (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;
    a.add_imm(body, proto, rcv);
    a.ldrb_imm(9, body, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, body, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, body, shape);
    a.mov_imm64(10, state.holder_shape as u64);
    a.cmp_reg_w(9, 10);
    a.b_cond(C_NE, fail);
    a.ldr_imm(10, body, entries_len);
    a.mov_imm64(13, state.slot as u64);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_HS, fail);
    a.ldr_imm(entry, body, entries);
    a.mov_imm64(10, layout.entry_size as u64);
    a.madd(entry, 13, 10, entry);
    guard_prop_data(a, 9, entry, layout.entry_accessor as u32, fail);
    a.ldur(13, entry, layout.entry_value as i32);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    a.mov_imm64(10, expected as u64);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_clone_rc(a: &mut asm::Asm, ptr: u32, strong: i32) {
    a.ldur(9, ptr, strong);
    a.add_imm(9, 9, 1);
    a.stur(9, ptr, strong);
}

/// Execute one complete IdleTask release without entering any of its three nested user
/// functions. All property/name/method/number checks precede the source-ordered three-write
/// commit, so the returned label can replay pc0 without duplicating observable effects.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_idle_release_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerIdleReleasePlan,
    l_ret_ok: usize,
) -> usize {
    let fail = a.new_label();
    let ev = layout.entry_value as i32;
    let strong = layout.rc_strong_off as i32;

    // x0 = the rooted IdleTask receiver. Count and v1 must be writable own exact integers;
    // count <= 1 includes the final hold iteration and deliberately replays from pc0.
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(0, 14, 8);
    emit_region_own_entry(a, layout, 0, 3, 5, plan.count, true, fail);
    emit_region_packed_number(a, 5, ev, 0, fail);
    emit_region_exact_i32(a, 0, 17, fail);
    let hot_count = a.new_label();
    a.cmp_imm_x(17, 1);
    a.b_cond(C_GT, hot_count);
    a.b(fail);
    a.bind(hot_count);
    emit_region_own_entry(a, layout, 0, 3, 6, plan.v1, true, fail);
    emit_region_packed_number(a, 6, ev, 2, fail);
    emit_region_exact_i32(a, 2, 17, fail);

    // Idle.scheduler must still be the warmed ordinary Scheduler, with the exact release
    // method on its immediate prototype and an own packed blocks array.
    emit_region_own_entry(a, layout, 0, 3, 4, plan.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.add_imm(14, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.release_method,
        plan.release_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 1, 3, 4, plan.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(2, 13, 16);
    a.lsr_imm(2, 2, 16);

    // Compute the selected branch exactly as JS ToInt32 arithmetic. The live ID binding is
    // guarded only for the branch actually taken, then indexes a packed own element.
    let odd = a.new_label();
    let selected = a.new_label();
    a.movz(9, 1, 0);
    a.logic_w(0, 9, 17, 9);
    a.cbnz(9, false, odd);
    a.asr_imm_w(17, 17, 1);
    a.scvtf_d_w(2, 17);
    emit_region_name_i32(a, layout, plan.id_a_cache, 7, fail);
    a.b(selected);
    a.bind(odd);
    a.asr_imm_w(17, 17, 1);
    a.movz(9, 0xD008, 0);
    a.logic_w(2, 17, 17, 9);
    a.scvtf_d_w(2, 17);
    emit_region_name_i32(a, layout, plan.id_b_cache, 7, fail);
    a.bind(selected);
    emit_scheduler_packed_target(a, layout, 2, 7, 3, fail);

    // Flatten markAsNotHeld, but keep its state value borrowed until every remaining guard has
    // passed. A target/current alias or a non-preempting comparison naturally declines here.
    a.add_imm(14, 3, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.mark_method,
        plan.mark_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 3, 14, 8, plan.state, true, fail);

    emit_region_own_entry(a, layout, 1, 14, 15, plan.current, false, fail);
    a.ldur(13, 15, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(4, 13, 16);
    a.lsr_imm(4, 4, 16);
    emit_region_own_entry(a, layout, 3, 14, 15, plan.target_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 4, fail);
    emit_region_own_entry(a, layout, 4, 14, 15, plan.current_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 5, fail);
    let precommit = a.new_label();
    a.fcmp(4, 5);
    a.b_cond(C_GT, precommit);
    a.b(fail);
    a.bind(precommit);

    emit_region_name_i32(a, layout, plan.not_held_cache, 7, fail);
    emit_region_packed_number(a, 8, ev, 0, fail);
    emit_region_exact_i32(a, 0, 17, fail);
    a.logic_w(0, 17, 17, 7);

    // Re-read count from its already-guarded entry after name-cache scratch clobbers, then commit
    // in source order: --count, v1 update, target.state &= STATE_NOT_HELD.
    emit_region_packed_number(a, 5, ev, 0, fail);
    emit_region_exact_i32(a, 0, 7, fail);
    a.sub_imm(7, 7, 1);
    a.scvtf_d_w(0, 7);
    a.fmov_x_d(13, 0);
    a.stur(13, 5, ev);
    a.fmov_x_d(13, 2);
    a.stur(13, 6, ev);
    a.scvtf_d_w(0, 17);
    a.fmov_x_d(13, 0);
    a.stur(13, 8, ev);

    // Return the selected target with exactly one new stack/return owner. Scheduler.currentTcb
    // is intentionally untouched: the outer scheduler call performs its ordinary assignment.
    emit_region_clone_rc(a, 3, strong);
    a.movz(9, 8, 0);
    a.stur(9, 20, 0);
    a.stur(3, 20, 8);
    a.add_imm(20, 20, 16);
    emit_helper(a, H_RETURN, 1);
    a.b(l_ret_ok);
    fail
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_commit(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDevicePlan,
    task: u32,
    target: usize,
    clear_fast_loop: bool,
) {
    a.add_imm(0, 22, plan.packet_off);
    a.add_imm(1, 22, plan.device_packet_off);
    a.add_imm(2, 22, plan.task_off);
    a.add_imm(3, 22, plan.temp_off);
    a.add_imm(4, task, layout.gc_data_off as u32);
    a.mov_imm64(
        16,
        jit_scheduler_device_materialize as *const () as usize as u64,
    );
    a.blr(16);
    if clear_fast_loop {
        a.movz(28, 0, 0);
    }
    a.b(target);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_loop_continue(
    a: &mut asm::Asm,
    fast_resume: Option<usize>,
    loop_pc: usize,
    pc_labels: &[usize],
) {
    if let Some(resume) = fast_resume {
        let ordinary = a.new_label();
        a.cbz(28, true, ordinary);
        a.sub_imm(28, 28, 1);
        a.cbz(28, true, ordinary);
        a.b(resume);
        a.bind(ordinary);
        a.movz(28, 0, 0);
    }
    a.b(pc_labels[loop_pc]);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_suspend(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDeviceSuspendPlan,
    core: Option<SchedulerGraphCoreContext>,
    method_epoch: bool,
    fast_resume: Option<usize>,
    fail: usize,
    pc_labels: &[usize],
) {
    let ev = layout.entry_value as i32;

    if let Some(core) = core {
        let ordinary = a.new_label();

        // CORE_VALID is published only after every exact graph task proved its own Scheduler
        // edge. x0/x26 come from the same freshly remapped Scheduler.current; keep that identity
        // check local so a future caller cannot accidentally dereference a stale native record.
        // x28 is the authoritative bounded-session publication/lifetime gate: header bits may
        // remain set in dead frame storage after any generic or user-code exit invalidates them.
        a.cbz(28, true, ordinary);
        a.ldr_imm(9, 31, SCHED_GRAPH_CORE_FLAGS_SP + core.sp_bias);
        a.movz(10, SCHED_GRAPH_CORE_VALID, 0);
        a.logic_w(0, 9, 9, 10);
        a.cbz(9, false, ordinary);
        a.ldr_imm(9, core.current_record, SCHED_GRAPH_TCB_OFF);
        a.cmp_reg_x(9, 0);
        a.b_cond(C_NE, ordinary);

        // Graph fill proved an exact writable numeric state entry, and no user code can run
        // while the bounded graph session is published. Every direct writer preserves an
        // integral Number, so suspend is now one cached load/OR/store transaction.
        a.ldr_imm(4, core.current_record, SCHED_GRAPH_STATE_ENTRY_OFF);
        a.ldur_d(0, 4, ev);
        a.fcvtzs_w_d(6, 0);
        a.sxtw(6, 6);
        a.ldr_imm(5, 31, SCHED_GRAPH_SUSPENDED_SP + core.sp_bias);
        a.logic_w(1, 6, 6, 5);
        a.scvtf_d_w(0, 6);
        a.fmov_x_d(13, 0);
        a.stur(13, 4, ev);
        emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);

        // A soft proof miss or defensive identity mismatch has made no observable change. Replay
        // the complete live guard chain below without discarding the valid base graph epoch.
        a.bind(ordinary);
    }

    // x0 is the active TCB and x5 is the exact DeviceTask. Its scheduler must be the outer
    // schedule() receiver; this also pins every receiver used by the flattened call chain.
    emit_region_own_entry(a, layout, 5, 3, 4, plan.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(2, 14, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    if method_epoch {
        // The session-entry proof belongs to the exact outer Scheduler pinned in x23. Task-local
        // scheduler fields remain live values, so keep the identity check at every transaction.
        a.cmp_reg_x(1, 23);
        a.b_cond(C_NE, fail);
    }

    // Guard the outer suspendCurrent method, its writable own current TCB, and the fact that
    // assigning its return value back to scheduler.current is an exact same-object no-op.
    emit_region_own_entry(a, layout, 1, 3, 4, plan.current, true, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(2, 13, 16);
    a.lsr_imm(2, 2, 16);
    a.cmp_reg_x(2, 0);
    a.b_cond(C_NE, fail);
    if !method_epoch {
        a.ldr_imm(8, 3, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.suspend_method,
            plan.suspend_expected,
            fail,
        );
    }

    // Inline markAsSuspended's exact bitwise state update. Both method identities, the live
    // global flag, numeric coercion, and writable data descriptor are proven before the store.
    emit_region_own_entry(a, layout, 0, 3, 4, plan.state, true, fail);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_exact_i32(a, 0, 6, fail);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    if method_epoch {
        // Every current TCB in a trusted continuation must retain the session-pinned prototype.
        a.cmp_reg_x(8, 25);
        a.b_cond(C_NE, fail);
    } else {
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.mark_method,
            plan.mark_expected,
            fail,
        );
    }
    emit_region_name_i32(a, layout, plan.suspended_cache, 5, fail);

    // --- commit: no fallible operation follows ---
    a.logic_w(1, 6, 6, 5);
    a.scvtf_d_w(0, 6);
    a.fmov_x_d(13, 0);
    a.stur(13, 4, ev);
    emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_hold(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDeviceHoldPlan,
    v1_entry: u32,
    v1_packed: u32,
    packet: u32,
    fast_resume: Option<usize>,
    fail: usize,
    pc_labels: &[usize],
) {
    let ev = layout.entry_value as i32;
    let strong = layout.rc_strong_off as i32;
    let outer_fail = fail;
    let fail = a.new_label();
    a.stp_pre(23, 24, -16);
    a.mov(23, v1_entry);
    a.mov(6, packet); // preserve the packet while x13-x16 serve as property-guard scratch

    // Keep the direct transaction to Richards' hot, allocation-free state: v1 is exactly Null
    // and writable, so storing the packet cannot release an old owner or invoke user code.
    a.mov_imm64(14, crate::value::PACK_NULL);
    a.cmp_reg_x(v1_packed, 14);
    a.b_cond(C_NE, fail);
    guard_prop_writable(
        a,
        9,
        23,
        layout.entry_writable as u32,
        fail,
    );

    // x0=active TCB, x5=DeviceTask. Guard Device.scheduler === outer schedule receiver and then
    // the exact holdCurrent method before consuming any facts from that method's child chunk.
    emit_region_own_entry(a, layout, 5, 3, 4, plan.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(2, 14, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    a.add_imm(3, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.hold_method,
        plan.hold_expected,
        fail,
    );
    a.mov(5, 6); // DeviceTask is dead; keep the packet in a low preserved register.

    // holdCurrent's currentTcb must be this exact TCB. Its eventual outer assignment is a live,
    // writable own data store; x2 retains the entry for the final commit.
    emit_region_own_entry(a, layout, 1, 3, 4, plan.current, true, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    a.cmp_reg_x(13, 0);
    a.b_cond(C_NE, fail);
    a.mov(2, 4);

    // Numeric ++ is an IEEE-754 add after ToNumeric. Restrict to an already-Number value, but
    // otherwise preserve NaN/infinity/fraction semantics rather than narrowing to i32.
    emit_region_own_entry(a, layout, 1, 3, 4, plan.hold_count, true, fail);
    emit_region_packed_number(a, 4, ev, 1, fail);
    a.fmov_one(2);
    a.f_arith(0, 1, 1, 2);
    a.fmov_x_d(24, 1);
    a.mov(6, 4);

    // Exact nested markAsHeld plus its live flag and writable numeric state.
    emit_region_own_entry(a, layout, 0, 3, 4, plan.state, true, fail);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.mark_method,
        plan.mark_expected,
        fail,
    );
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_exact_i32(a, 0, 3, fail);
    a.mov(8, 4);
    emit_region_name_i32(a, layout, plan.held_cache, 4, fail);
    a.logic_w(1, 3, 3, 4); // committed state bits, retained in w3

    // Read the return link last. Only exact Null or Obj is specialized. A different Obj needs a
    // clone; replacing current drops one TCB owner, so prove that drop cannot run a destructor.
    emit_region_own_entry(a, layout, 0, 14, 15, plan.link, false, fail);
    a.ldur(12, 15, ev);
    let link_null = a.new_label();
    let link_same = a.new_label();
    let guards_done = a.new_label();
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, link_null);
    a.lsr_imm(9, 12, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(4, 12, 16);
    a.lsr_imm(4, 4, 16);
    if fast_resume.is_some() {
        // holdCurrent selects current.link. Keep a trusted continuation only when that linked
        // TCB shares the session-pinned prototype whose run method was proved at entry.
        a.add_imm(14, 4, layout.obj_from_rc as u32);
        a.ldr_imm(9, 14, layout.obj_proto as u32);
        a.cmp_reg_x(9, 25);
        a.b_cond(C_NE, fail);
    }
    a.cmp_reg_x(4, 0);
    a.b_cond(C_EQ, link_same);
    a.ldur(9, 0, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, fail);
    a.b(guards_done);

    a.bind(link_null);
    a.movz(4, 0, 0); // zero marks Null at commit
    a.ldur(9, 0, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, fail);
    a.b(guards_done);

    a.bind(link_same);
    a.mov(4, 0); // exact same-owner assignment is an RC no-op
    a.bind(guards_done);

    // --- commit: v1=packet; ++holdCount; state|=HELD; current=link ---
    emit_region_clone_rc(a, 5, strong);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 5);
    a.stur(13, 23, ev);
    a.stur(24, 6, ev);
    a.scvtf_d_w(0, 3);
    a.fmov_x_d(13, 0);
    a.stur(13, 8, ev);
    a.cmp_reg_x(4, 0);
    let current_done = a.new_label();
    let current_null = a.new_label();
    a.b_cond(C_EQ, current_done);
    a.cbz(4, true, current_null);
    emit_region_clone_rc(a, 4, strong);
    a.stur(12, 2, ev);
    a.ldur(9, 0, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);
    a.b(current_done);
    a.bind(current_null);
    a.stur(12, 2, ev); // x12 is the guarded PACK_NULL word
    a.ldur(9, 0, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);
    a.bind(current_done);
    a.ldp_post(23, 24, 16);
    emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);
    a.bind(fail);
    a.ldp_post(23, 24, 16);
    a.b(outer_fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_packed_entry(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    array: u32,
    index: u32,
    out: u32,
    fail: usize,
) {
    let elems = (layout.obj_props + layout.props_elems) as u32;
    a.add_imm(14, array, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 14, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_imm(15, 14, elems);
    a.cbz(15, true, fail);
    a.ldr_imm(15, 15, layout.dense_packed as u32);
    a.cbz(15, true, fail);
    a.ldr_imm(16, 15, layout.vec_len_off as u32);
    a.cmp_reg_x(index, 16);
    a.b_cond(C_HS, fail);
    a.ldr_imm(15, 15, layout.vec_ptr_off as u32);
    a.add_shifted(out, 15, index, 4);
    guard_prop_data(a, 9, out, layout.property_meta as u32, fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_packed_target(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    array: u32,
    index: u32,
    out: u32,
    fail: usize,
) {
    emit_scheduler_packed_entry(a, layout, array, index, 15, fail);
    a.ldur(13, 15, layout.property_value as i32);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(out, 13, 16);
    a.lsr_imm(out, out, 16);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_queue(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDeviceQueuePlan,
    source_entry: u32,
    packet: u32,
    source_update: SchedulerQueueSource,
    current_is_active: bool,
    method_epoch: bool,
    fast_resume: Option<usize>,
    fail: usize,
    pc_labels: &[usize],
) {
    let strong = layout.rc_strong_off as i32;
    let ev = layout.entry_value as i32;
    let outer_fail = fail;
    let fail = a.new_label();
    a.stp_pre(23, 24, -48);
    a.stp_off(25, 26, 16);
    a.stp_off(27, 28, 32);
    a.mov(23, source_entry);
    a.mov(24, packet);
    guard_prop_writable(
        a,
        9,
        23,
        layout.entry_writable as u32,
        fail,
    );

    // Exact Device.scheduler === the outer schedule receiver, followed by the live queue method.
    emit_region_own_entry(a, layout, 5, 3, 4, plan.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(2, 14, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    if method_epoch {
        // The spill's x23 is now the source entry; sp+0 retains the epoch-pinned Scheduler.
        a.ldr_imm(9, 31, 0);
        a.cmp_reg_x(1, 9);
        a.b_cond(C_NE, fail);
    } else {
        a.add_imm(3, 1, layout.obj_from_rc as u32);
        a.ldr_imm(8, 3, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.queue_method,
            plan.queue_expected,
            fail,
        );
    }

    // packet.id selects an own packed element of scheduler.blocks. The array and element guards
    // reject holes, prototype lookup, side tables, accessors, and non-object targets.
    emit_region_own_entry(a, layout, 1, 3, 4, plan.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    emit_region_own_entry(a, layout, 24, 14, 15, plan.packet_id, true, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    a.fcvtzu_w_d(6, 0);
    a.ucvtf_d_w(1, 6);
    a.fcmp(0, 1);
    a.b_cond(C_NE, fail);
    a.mov(4, 15);
    emit_scheduler_packed_target(a, layout, 8, 6, 25, fail);

    // Queue counter and packet writes. packet.link is Null or an object on the hot path. Device
    // clears its source and therefore releases an old link owner at commit; Handler instead moves
    // that owner into its source entry without changing the strong count. currentId and id are
    // both restricted to Number values.
    emit_region_own_entry(a, layout, 1, 14, 15, plan.queue_count, true, fail);
    emit_region_packed_number(a, 15, ev, 2, fail);
    a.fmov_one(1);
    a.f_arith(0, 2, 2, 1);
    a.mov(26, 15);
    emit_region_own_entry(a, layout, 24, 14, 15, plan.packet_link, true, fail);
    a.mov(2, 15); // packet.link entry, kept for the commit
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let link_ready = a.new_label();
    let link_object = a.new_label();
    a.b_cond(C_NE, link_object);
    match source_update {
        SchedulerQueueSource::Clear => {
            a.movz(5, 0, 0); // zero means there is no old owner to release
        }
        SchedulerQueueSource::AdvanceToPacketLink => {
            a.mov(5, 13); // preserve the packed Null word for the source entry
        }
    }
    a.b(link_ready);
    a.bind(link_object);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    match source_update {
        SchedulerQueueSource::Clear => {
            a.lsl_imm(5, 13, 16);
            a.lsr_imm(5, 5, 16);
            // The source is cleared, so the old link owner must be releasable without invoking a
            // destructor from generated code. Handler transfers this owner instead and needs no
            // corresponding count guard.
            a.ldur(9, 5, strong);
            a.cmp_imm_x(9, 1);
            a.b_cond(C_LS, fail);
        }
        SchedulerQueueSource::AdvanceToPacketLink => {
            a.mov(5, 13); // transfer the exact packed owner into the source entry at commit
        }
    }
    a.bind(link_ready);
    emit_region_own_entry(a, layout, 1, 14, 15, plan.current_id, false, fail);
    emit_region_packed_number(a, 15, ev, 3, fail);

    // queue() passes the exact current TCB into target.checkPriorityAdd. Device enters with that
    // TCB already in x0; a Handler region entered deeper in the bytecode materializes it here.
    // The returned non-preempting current is assigned to scheduler.current, so prove the writable
    // same-object no-op up front.
    emit_region_own_entry(a, layout, 1, 14, 15, plan.current, true, fail);
    a.ldur(13, 15, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    if current_is_active {
        a.cmp_reg_x(13, 0);
        a.b_cond(C_NE, fail);
    } else {
        a.mov(0, 13);
    }

    a.add_imm(14, 25, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    if method_epoch {
        // x25 is the blocks-derived target inside this spill; sp+16 retains the epoch's common
        // TCB prototype. A foreign target takes the untouched precommit replay path.
        a.ldr_imm(9, 31, 16);
        a.cmp_reg_x(8, 9);
        a.b_cond(C_NE, fail);
    } else {
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.check_method,
            plan.check_expected,
            fail,
        );
    }

    // Empty target.queue owns the packet after commit. The exact nested markAsRunnable target,
    // writable state, and live flag cover the only side effect in this checkPriorityAdd arm.
    emit_region_own_entry(a, layout, 25, 14, 15, plan.target_queue, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.mov(27, 15);
    if !method_epoch {
        a.add_imm(14, 25, layout.obj_from_rc as u32);
        a.ldr_imm(8, 14, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.mark_method,
            plan.mark_expected,
            fail,
        );
    }
    emit_region_own_entry(a, layout, 25, 14, 15, plan.state, true, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 6, fail);
    a.mov(28, 15);
    emit_region_name_i32(a, layout, plan.runnable_cache, 7, fail);
    a.logic_w(1, 6, 6, 7);

    // The benchmark's target priority never preempts the current Device TCB. A true comparison
    // replays before mutation so the baseline returns the target and performs the outer store.
    emit_region_own_entry(a, layout, 25, 14, 15, plan.target_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 4, fail);
    emit_region_own_entry(a, layout, 0, 14, 15, plan.current_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 5, fail);
    a.fcmp(4, 5);
    a.b_cond(C_GT, fail);

    // --- commit: transfer the source packet owner to target.queue, then publish updates ---
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 24);
    a.stur(13, 27, ev);
    match source_update {
        SchedulerQueueSource::Clear => {
            a.mov_imm64(13, crate::value::PACK_NULL);
            a.stur(13, 23, ev);
        }
        SchedulerQueueSource::AdvanceToPacketLink => {
            a.stur(5, 23, ev);
        }
    }
    a.fmov_x_d(13, 2);
    a.stur(13, 26, ev);
    a.fmov_x_d(13, 3);
    a.stur(13, 4, ev);
    a.scvtf_d_w(0, 6);
    a.fmov_x_d(13, 0);
    a.stur(13, 28, ev);
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.stur(13, 2, ev);
    if matches!(source_update, SchedulerQueueSource::Clear) {
        let link_released = a.new_label();
        a.cbz(5, true, link_released);
        a.ldur(9, 5, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 5, strong);
        a.bind(link_released);
    }
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);

    a.bind(fail);
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    a.b(outer_fail);
}

/// Flatten HandlerTask's completed-work arm beginning at its empty-stack pc. Handler.v1 owns the
/// packet and packet.link owns the successor; the shared queue emitter transfers those owners to
/// target.queue and Handler.v1 respectively without touching either strong count.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_queue_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerQueuePlan,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let ev = layout.entry_value as i32;

    a.ldrb_imm(9, 22, plan.handler_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(5, 22, plan.handler_off + 8);
    emit_region_own_entry(a, layout, 5, 6, 7, plan.v1, true, fail);
    a.ldur(13, 7, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    emit_scheduler_device_queue(
        a,
        layout,
        &plan.queue,
        7,
        13,
        SchedulerQueueSource::AdvanceToPacketLink,
        false,
        false,
        None,
        fail,
        pc_labels,
    );
    fail
}

/// Flatten HandlerTask's numeric v2 delivery into the empty/preempting or one-node Packet.addTo
/// arm. Packet/source ownership is moved without count traffic; only replacing a preempted
/// Scheduler.current needs a guarded target retain and old-current release.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_deliver_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerDeliverPlan,
    pc_labels: &[usize],
) -> usize {
    emit_scheduler_handler_deliver_transaction(
        a,
        layout,
        plan,
        SchedulerHandlerDeliverSource::Locals,
        None,
        None,
        pc_labels,
    )
}

/// Emit the common delivery guards and commit. The stitched sources consume values already
/// proven by their Handler prefixes. `IncomingDevice` deliberately leaves skipped compiler
/// locals alone: they are fixed frame slots, dead on success, and overwritten later or released
/// with the schedule frame, so retention is bounded rather than iteration-cumulative.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_deliver_transaction(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerDeliverPlan,
    source: SchedulerHandlerDeliverSource,
    active: Option<(&SchedulerActivePlan, u32, u32)>,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let outer_fail = a.new_label();
    let fail = a.new_label();
    let ev = layout.entry_value as i32;
    let pv = layout.property_value as i32;
    let strong = layout.rc_strong_off as i32;
    let active_work = matches!(source, SchedulerHandlerDeliverSource::ActiveIncomingWork);
    let active = if active_work {
        Some(active.expect("Active incoming WORK source requires Active plan"))
    } else {
        debug_assert!(active.is_none());
        None
    };

    // Saved x23..x28 occupy 0..48 and delivery entries/source state occupy 48..112.
    // No call or guard follows the commit.
    a.stp_pre(23, 24, -112);
    a.stp_off(25, 26, 16);
    a.stp_off(27, 28, 32);

    match source {
        SchedulerHandlerDeliverSource::Locals => {
            a.ldrb_imm(9, 22, plan.handler_off);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, fail);
            a.ldr_imm(5, 22, plan.handler_off + 8);
            emit_region_own_entry(a, layout, 5, 6, 23, plan.v2, true, fail);
            a.ldur(13, 23, ev);
            a.lsr_imm(9, 13, 48);
            a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
            a.cmp_reg_x(9, 10);
            a.b_cond(C_NE, fail);
            a.lsl_imm(24, 13, 16);
            a.lsr_imm(24, 24, 16);
        }
        SchedulerHandlerDeliverSource::ActiveNull => {
            // The Active prefix already proved this exact writable Handler.v2 entry while
            // selecting P as an object. No user code or mutation runs before this transaction.
            a.mov(23, 3);
            a.mov(24, 6);
        }
        SchedulerHandlerDeliverSource::IncomingDevice => {
            // pc59 already proved exact Handler.v2/P.link writable Null entries. Preserve their
            // pointers only so the common spill layout stays identical; both no-op stores are
            // omitted at commit and the incoming packet remains owned by its active-frame local.
            a.mov(23, 3);
            a.mov(24, 6);
            a.stur(15, 31, 48);
            a.mov_imm64(28, crate::value::PACK_NULL);
            a.stur(28, 31, 104);
        }
        SchedulerHandlerDeliverSource::ActiveIncomingWork => {
            // The enclosing Active Handler prefix proved writable H.v2 and its exact object head
            // D. Handler.v1 is still Null and the incoming work packet W remains virtual.
            a.mov(23, 3);
            a.mov(24, 6);
        }
    }

    if matches!(
        source,
        SchedulerHandlerDeliverSource::Locals
            | SchedulerHandlerDeliverSource::ActiveNull
            | SchedulerHandlerDeliverSource::ActiveIncomingWork
    ) {
        // L = P.link. The packed L word remains in x28 until ownership commit; a successor
        // object is accepted by both ordinary and Active-null delivery.
        emit_region_own_entry(a, layout, 24, 14, 15, plan.packet_link, true, fail);
        a.stur(15, 31, 48);
        a.ldur(28, 15, ev);
        a.mov_imm64(10, crate::value::PACK_NULL);
        a.cmp_reg_x(28, 10);
        let source_link_ready = a.new_label();
        let source_link_object = a.new_label();
        a.b_cond(C_NE, source_link_object);
        a.b(source_link_ready);
        a.bind(source_link_object);
        a.lsr_imm(9, 28, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.bind(source_link_ready);
        a.stur(28, 31, 104);
    }

    // W = Handler.v1. Canonical Richards keeps W and P distinct; aliasing replays so the ordinary
    // bytecode preserves its observable P.a1-then-W.a1 write order.
    if active_work {
        a.mov(25, 8); // virtual post-add Handler.v1 is incoming W
    } else {
        emit_region_own_entry(a, layout, 5, 14, 15, plan.v1, false, fail);
        a.ldur(13, 15, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(25, 13, 16);
        a.lsr_imm(25, 25, 16);
    }
    a.cmp_reg_x(25, 24);
    a.b_cond(C_EQ, fail);
    if active_work {
        // Reject a D.link successor that aliases any object whose owner is moved by the folded
        // Active/Handler transaction. Null is the canonical tail and needs no pointer checks.
        let delivery_link_distinct = a.new_label();
        a.ldur(13, 31, 104);
        a.mov_imm64(10, crate::value::PACK_NULL);
        a.cmp_reg_x(13, 10);
        a.b_cond(C_EQ, delivery_link_distinct);
        a.lsl_imm(7, 13, 16);
        a.lsr_imm(7, 7, 16);
        for reg in [24u32, 25, 5] {
            a.cmp_reg_x(7, reg);
            a.b_cond(C_EQ, fail);
        }
        for off in [112i32, 120] {
            a.ldur(9, 31, off);
            a.cmp_reg_x(7, 9);
            a.b_cond(C_EQ, fail);
        }
        a.bind(delivery_link_distinct);
    }

    // Read the already-materialized count local: re-reading W.a1 or DATA_SIZE here would repeat
    // getters that ran before this replay boundary. Only exact nonnegative int32 packed-array
    // indexes and numeric payloads enter the transaction.
    emit_region_own_entry(a, layout, 25, 14, 15, plan.payload_array, false, fail);
    a.ldur(13, 15, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    match source {
        SchedulerHandlerDeliverSource::Locals => {
            a.ldrb_imm(9, 22, plan.count_off);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, fail);
            a.ldr_d_imm(5, 22, plan.count_off + 8);
            emit_region_exact_i32(a, 5, 6, fail);
        }
        SchedulerHandlerDeliverSource::ActiveNull
        | SchedulerHandlerDeliverSource::IncomingDevice
        | SchedulerHandlerDeliverSource::ActiveIncomingWork => {
            // The virtual Handler prefix left the exact Handler.v1.a1 int32 in x2. Reconstruct
            // its numeric representation without touching the skipped count local.
            a.mov(6, 2);
            a.scvtf_d_w(5, 6);
        }
    }
    a.cmp_imm_w(6, 0);
    a.b_cond(C_MI, fail);
    emit_scheduler_packed_entry(a, layout, 8, 6, 15, fail);
    emit_region_packed_number(a, 15, pv, 4, fail);

    // Both overwritten a1 fields must already be numeric; this makes payload/cursor stores pure
    // bit writes with no old-owner release. d5 becomes count+1 after its exact-int proof above.
    emit_region_own_entry(a, layout, 24, 14, 15, plan.packet_a1, true, fail);
    a.stur(15, 31, 56);
    emit_region_packed_number(a, 15, ev, 0, fail);
    if active_work {
        // The enclosing prefix already proved this exact W.a1 entry writable and numeric before
        // entering the common transaction; x4 survives the guards above until it is spilled.
        a.stur(4, 31, 64);
    } else {
        emit_region_own_entry(a, layout, 25, 14, 15, plan.work_a1, true, fail);
        a.stur(15, 31, 64);
        emit_region_packed_number(a, 15, ev, 0, fail);
    }
    a.fmov_one(0);
    a.f_arith(0, 5, 5, 0);

    // Exact Handler.scheduler === the active schedule receiver and exact live queue method.
    emit_region_own_entry(a, layout, 5, 3, 4, plan.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(2, 14, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    a.add_imm(3, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.queue_method,
        plan.queue_expected,
        fail,
    );

    // scheduler.blocks[P.id] -> exact object target.
    emit_region_own_entry(a, layout, 1, 3, 4, plan.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    emit_region_own_entry(a, layout, 24, 14, 15, plan.packet_id, true, fail);
    a.stur(15, 31, 80);
    emit_region_packed_number(a, 15, ev, 0, fail);
    a.fcvtzu_w_d(6, 0);
    a.ucvtf_d_w(1, 6);
    a.fcmp(0, 1);
    a.b_cond(C_NE, fail);
    emit_scheduler_packed_target(a, layout, 8, 6, 26, fail);

    emit_region_own_entry(a, layout, 1, 14, 15, plan.queue_count, true, fail);
    a.stur(15, 31, 72);
    emit_region_packed_number(a, 15, ev, 2, fail);
    a.fmov_one(0);
    a.f_arith(0, 2, 2, 0);
    emit_region_own_entry(a, layout, 1, 14, 15, plan.current_id, false, fail);
    if active_work {
        // Active's pending `currentId = C.id` has not been published yet. Guard the same entry,
        // but derive queue()'s packet id from the saved exact packed C.id.
        a.ldur(9, 31, 152);
        a.cmp_reg_x(15, 9);
        a.b_cond(C_NE, fail);
        emit_region_packed_scalar(a, 15, ev, 13, fail);
        a.ldur(13, 31, 144);
        a.fmov_d_x(3, 13);
    } else {
        emit_region_packed_number(a, 15, ev, 3, fail);
    }

    // The nonempty check arm returns the exact current value unchanged. Requiring an object and a
    // writable own scheduler entry makes the nonempty outer assignment an observable no-op and
    // supplies the entry replaced by the empty/preempting arm.
    emit_region_own_entry(a, layout, 1, 14, 15, plan.current, true, fail);
    a.stur(15, 31, 88);
    a.ldur(13, 15, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(0, 13, 16);
    a.lsr_imm(0, 0, 16);
    if active_work {
        a.ldur(9, 31, 112);
        a.cmp_reg_x(0, 9);
        a.b_cond(C_NE, fail);
        for reg in [24u32, 25, 5] {
            a.cmp_reg_x(26, reg);
            a.b_cond(C_EQ, fail);
        }
        for off in [112i32, 120] {
            a.ldur(9, 31, off);
            a.cmp_reg_x(26, 9);
            a.b_cond(C_EQ, fail);
        }
        let target_link_distinct = a.new_label();
        a.ldur(13, 31, 104);
        a.mov_imm64(10, crate::value::PACK_NULL);
        a.cmp_reg_x(13, 10);
        a.b_cond(C_EQ, target_link_distinct);
        a.lsl_imm(7, 13, 16);
        a.lsr_imm(7, 7, 16);
        a.cmp_reg_x(26, 7);
        a.b_cond(C_EQ, fail);
        a.bind(target_link_distinct);
    }

    a.add_imm(14, 26, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.check_method,
        plan.check_expected,
        fail,
    );

    // Empty target.queue takes the preempting checkPriorityAdd arm; an object takes the dominant
    // one-node Packet.addTo arm. All path-specific guards still precede the shared first store.
    emit_region_own_entry(a, layout, 26, 14, 15, plan.target_queue, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let nonempty_queue = a.new_label();
    let commit_common = a.new_label();
    if plan.empty_preempt {
        a.b_cond(C_NE, nonempty_queue);
    } else {
        a.b_cond(C_EQ, fail);
        a.b(nonempty_queue);
    }

    // Empty/preempting: target.queue receives P, markAsRunnable updates an exact numeric state,
    // and the exact live priorities prove that checkPriorityAdd returns the target. Preserve the
    // target queue/state entries in x27/x28 and the already-spilled current entry at sp+88.
    a.mov(27, 15);
    a.add_imm(14, 26, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.mark_method,
        plan.mark_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 26, 14, 15, plan.state, true, fail);
    a.mov(28, 15);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 6, fail);
    emit_region_name_i32(a, layout, plan.runnable_cache, 7, fail);
    a.logic_w(1, 6, 6, 7);

    // Identical target/current objects cannot preempt, but reject the alias explicitly before the
    // refcount proof. Ordered floating greater also rejects NaN, matching Number `>` semantics.
    a.cmp_reg_x(26, 0);
    a.b_cond(C_EQ, fail);
    emit_region_own_entry(a, layout, 26, 14, 15, plan.target_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 6, fail);
    emit_region_own_entry(a, layout, 0, 14, 15, plan.current_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 7, fail);
    a.fcmp(6, 7);
    let ordered_preempt = a.new_label();
    a.b_cond(C_GT, ordered_preempt);
    a.b(fail);
    a.bind(ordered_preempt);
    a.ldur(9, 0, strong);
    if active_work {
        let old_tcb_distinct = a.new_label();
        let old_packet_distinct = a.new_label();
        a.movz(10, 1, 0); // Scheduler.current's later decrement
        a.ldur(13, 31, 208);
        a.cmp_reg_x(13, 0);
        a.b_cond(C_NE, old_tcb_distinct);
        a.add_imm(10, 10, 1);
        a.bind(old_tcb_distinct);
        a.ldur(13, 31, 216);
        a.cmp_reg_x(13, 0);
        a.b_cond(C_NE, old_packet_distinct);
        a.add_imm(10, 10, 1);
        a.bind(old_packet_distinct);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_LS, fail);
    } else {
        a.cmp_imm_x(9, 1);
        a.b_cond(C_LS, fail);
    }
    a.movz(12, 0, 0); // zero selects the empty/preempt commit tail
    a.b(commit_common);

    // Q = target.queue must be an exact Packet-shaped object distinct from P/W. Valid uncommon
    // aliases replay so ordinary bytecode preserves their source-order overwrite semantics.
    a.bind(nonempty_queue);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(27, 13, 16);
    a.lsr_imm(27, 27, 16);
    a.cmp_reg_x(27, 24);
    a.b_cond(C_EQ, fail);
    a.cmp_reg_x(27, 25);
    a.b_cond(C_EQ, fail);
    if active_work {
        a.cmp_reg_x(27, 5);
        a.b_cond(C_EQ, fail);
        for off in [112i32, 120] {
            a.ldur(9, 31, off);
            a.cmp_reg_x(27, 9);
            a.b_cond(C_EQ, fail);
        }
        let queued_link_distinct = a.new_label();
        a.ldur(13, 31, 104);
        a.mov_imm64(10, crate::value::PACK_NULL);
        a.cmp_reg_x(13, 10);
        a.b_cond(C_EQ, queued_link_distinct);
        a.lsl_imm(7, 13, 16);
        a.lsr_imm(7, 7, 16);
        a.cmp_reg_x(27, 7);
        a.b_cond(C_EQ, fail);
        a.bind(queued_link_distinct);
    }
    a.add_imm(14, 24, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.add_method,
        plan.add_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 27, 14, 15, plan.queued_link, true, fail);
    a.stur(15, 31, 96);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.movz(12, 1, 0); // nonzero selects the one-node append commit tail
    a.b(commit_common);

    // --- commit in source order ---
    a.bind(commit_common);
    match source {
        SchedulerHandlerDeliverSource::Locals | SchedulerHandlerDeliverSource::ActiveNull => {
            a.ldur(13, 31, 104);
            a.stur(13, 23, ev); // Handler.v2 = old P.link; its P owner moves below
        }
        SchedulerHandlerDeliverSource::IncomingDevice => {
            // Handler.v2 and P.link both began and end Null. Clone the active packet local's
            // owner exactly once for the destination instead of creating transient inline-frame
            // owners or publishing P through Handler.v2 first.
            emit_region_clone_rc(a, 24, strong);
        }
        SchedulerHandlerDeliverSource::ActiveIncomingWork => {
            // Publish virtual post-add Handler.v1 before removing C.queue's incoming-W owner.
            // The exact saved W.link is Null for this narrow source, but move the observed word
            // into C.queue so the ownership order remains explicit and count-neutral.
            a.mov_imm64(13, crate::value::PACK_OBJ);
            a.logic_x(1, 13, 13, 25);
            a.ldur(15, 31, 224);
            a.stur(13, 15, ev);
            a.ldur(13, 31, 184);
            a.ldur(15, 31, 160);
            a.stur(13, 15, ev);
        }
    }
    a.ldur(15, 31, 56);
    a.fmov_x_d(13, 4);
    a.stur(13, 15, ev); // P.a1 = W.a2[count]
    a.ldur(15, 31, 64);
    a.fmov_x_d(13, 5);
    a.stur(13, 15, ev); // W.a1 = count + 1
    a.ldur(15, 31, 72);
    a.fmov_x_d(13, 2);
    a.stur(13, 15, ev); // ++queueCount
    if matches!(
        source,
        SchedulerHandlerDeliverSource::Locals | SchedulerHandlerDeliverSource::ActiveNull
    ) {
        a.ldur(15, 31, 48);
        a.mov_imm64(13, crate::value::PACK_NULL);
        a.stur(13, 15, ev); // P.link = null
    }
    a.ldur(15, 31, 80);
    a.fmov_x_d(13, 3);
    a.stur(13, 15, ev); // P.id = currentId
    let commit_empty = a.new_label();
    let commit_done = a.new_label();
    a.cbz(12, true, commit_empty);
    a.ldur(15, 31, 96);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 24);
    a.stur(13, 15, ev); // Q.link = P
    a.b(commit_done);

    a.bind(commit_empty);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 24);
    a.stur(13, 27, ev); // target.queue = P (moves Handler.v2's former owner)
    a.scvtf_d_w(0, 6);
    a.fmov_x_d(13, 0);
    a.stur(13, 28, ev); // target.state |= STATE_RUNNABLE
    if !active_work {
        emit_region_clone_rc(a, 26, strong);
        a.mov_imm64(13, crate::value::PACK_OBJ);
        a.logic_x(1, 13, 13, 26);
        a.ldur(15, 31, 88);
        a.stur(13, 15, ev); // scheduler.current = target
        a.ldur(9, 0, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 0, strong);
    }

    a.bind(commit_done);
    if let Some((active, state_entry, tcb_proto)) = active {
        // The target now owns D, so it is safe to move D.link into H.v2 and clear both link
        // sources. All guards are complete; no call or failure edge follows these first stores.
        a.ldur(13, 31, 104);
        a.stur(13, 23, ev); // H.v2 = old D.link
        a.mov_imm64(13, crate::value::PACK_NULL);
        a.ldur(15, 31, 232);
        a.stur(13, 15, ev); // incoming W.link = Null
        a.ldur(15, 31, 48);
        a.stur(13, 15, ev); // delivered D.link = Null

        a.ldur(13, 31, 144);
        a.ldur(15, 31, 152);
        a.stur(13, 15, ev); // Scheduler.currentId = packed C.id
        a.ldur(6, 31, 128);
        a.scvtf_d_w(0, 6);
        a.fmov_x_d(13, 0);
        a.ldur(15, 31, 192);
        a.stur(13, 15, ev); // C.state = pending STATE_RUNNING

        // Successful stitched iterations retain no skipped compiler-local graph. Counts were
        // proven non-last, including any C aliases used by the preempt threshold above.
        a.movz(9, 0, 0); // Value::Undefined
        a.str_imm(9, 22, active.tcb_off);
        a.str_imm(31, 22, active.tcb_off + 8);
        a.movz(9, 2, 0); // Value::Null
        a.str_imm(9, 22, active.packet_off);
        a.str_imm(31, 22, active.packet_off + 8);
        a.ldp_off(6, 7, 208);
        emit_scheduler_active_drop_old_locals(a, layout, 6, 7);

        let current_done = a.new_label();
        a.cbnz(12, false, current_done);
        emit_region_clone_rc(a, 26, strong);
        a.mov_imm64(13, crate::value::PACK_OBJ);
        a.logic_x(1, 13, 13, 26);
        a.ldur(15, 31, 88);
        a.stur(13, 15, ev); // Scheduler.current = preempting target
        a.ldur(9, 0, strong);
        a.sub_imm(9, 9, 1);
        a.stur(9, 0, strong); // C is not used after its final owner release
        a.bind(current_done);

        // Preserve empty/nonempty across the delivery, existing 192-byte Handler, and enclosing
        // 48-byte Active snapshots. Nonempty keeps current and may fast-resume; preempt clears the
        // graph epoch and rebuilds from the canonical scheduler loop.
        a.mov(17, 12);
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 112);
        a.ldp_off(11, 12, 16);
        a.ldp_off(23, 24, 32);
        a.ldp_off(25, 26, 48);
        a.ldp_off(27, 28, 64);
        a.ldp_off(state_entry, tcb_proto, 80);
        a.ldp_post(0, 1, 192);
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 48);
        if fast_resume.is_some() {
            let canonical = a.new_label();
            a.cbz(17, true, canonical);
            emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);
            a.bind(canonical);
            a.movz(28, 0, 0);
        }
        a.b(pc_labels[plan.loop_pc]);
    }
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 112);
    if matches!(
        source,
        SchedulerHandlerDeliverSource::ActiveNull
            | SchedulerHandlerDeliverSource::IncomingDevice
    ) {
        if fast_resume.is_some() {
            // A one-node append leaves Scheduler.current and the active TCB untouched, and no
            // call ran after the task-role epoch guards. Reuse those facts for the next scheduler
            // iteration. Empty/preempting delivery publishes a different current and must
            // rebuild from canonical pc2 instead.
            let canonical = a.new_label();
            a.cbz(12, true, canonical);
            emit_scheduler_loop_continue(a, fast_resume, plan.loop_pc, pc_labels);
            a.bind(canonical);
            a.movz(28, 0, 0);
        }
    }
    a.b(pc_labels[plan.loop_pc]);

    a.bind(fail);
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 112);
    a.b(outer_fail);
    outer_fail
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_wait_prefix(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerSuspendPlan,
    task_prevalidated: bool,
    fail: usize,
) {
    let ev = layout.entry_value as i32;

    if !task_prevalidated {
        a.ldrb_imm(9, 22, plan.tcb_off);
        a.cmp_imm_w(9, 8);
        a.b_cond(C_NE, fail);
        a.ldr_imm(0, 22, plan.tcb_off + 8);
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
    }

    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 5, 6, 7, plan.v1, false, fail);
    } else {
        emit_region_own_entry(a, layout, 5, 6, 7, plan.v1, false, fail);
    }
    a.ldur(13, 7, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let no_work = a.new_label();
    let v1_object = a.new_label();
    a.b_cond(C_NE, v1_object);
    a.movz(3, 0, 0); // the v1-Null arm may only take the ordinary suspend path
    a.b(no_work);

    // A nonempty v1 still suspends when its numeric a1 cursor is below the live DATA_SIZE and
    // v2 is exact Null. This is the common Handler wait state; no Handler property is modified.
    a.bind(v1_object);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    emit_region_own_entry(a, layout, 8, 14, 15, plan.packet_a1, false, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 2, fail);
    emit_region_name_i32(a, layout, plan.data_size_cache, 7, fail);
    a.cmp_reg_w(2, 7);
    a.b_cond(C_GE, fail);
    emit_region_own_entry(a, layout, 5, 14, 15, plan.v2, false, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.movz(3, 1, 0); // v1 object/count/v2 facts authorize the incoming-device bridge

    a.bind(no_work);
    if !task_prevalidated {
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }
}

/// Bypass HandlerTask's inlined body when the incoming packet and its v1 worklist are both exact
/// Null. The task has no side effect before `suspendCurrent`, whose guarded transaction is shared
/// with DeviceTask. Failure falls through to the original polymorphic task dispatch at `head`.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_suspend_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerSuspendPlan,
    prevalidated_entry: Option<usize>,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let ev = layout.entry_value as i32;

    // The ordinary entry reconstructs the full receiver chain. The pc59 selector may instead
    // enter a compact v1/classification prefix; both paths converge before the transaction body.
    emit_scheduler_handler_wait_prefix(a, layout, plan, false, fail);
    if let Some(prevalidated_entry) = prevalidated_entry {
        let body = a.new_label();
        a.b(body);
        a.bind(prevalidated_entry);
        emit_scheduler_handler_wait_prefix(a, layout, plan, true, fail);
        a.bind(body);
    }

    let packet_null = a.new_label();
    a.ldrb_imm(9, 22, plan.packet_off);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_EQ, packet_null);
    let Some(bridge) = &plan.incoming else {
        a.b(fail);
        a.bind(packet_null);
        emit_scheduler_device_suspend(
            a,
            layout,
            &plan.suspend,
            None,
            false,
            fast_resume,
            fail,
            pc_labels,
        );
        return fail;
    };

    // The source loose-null check reaches this arm only for an ordinary object. Requiring an own
    // exact-int kind is a safe subset of `packet.kind != KIND_WORK`, and also excludes HTMLDDA.
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.cmp_imm_w(3, 1);
    a.b_cond(C_NE, fail);
    a.ldr_imm(6, 22, plan.packet_off + 8);
    emit_region_own_entry(a, layout, 6, 14, 15, bridge.kind, false, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 4, fail);
    emit_region_name_i32(a, layout, bridge.kind_work_cache, 7, fail);
    a.cmp_reg_w(4, 7);
    a.b_cond(C_EQ, fail);

    // Packet.addTo must still be the exact inlined one-argument method. With both its argument
    // (Handler.v2) and the incoming link Null, its only store is an unobservable Null no-op and it
    // returns the incoming packet itself.
    a.add_imm(14, 6, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        bridge.add_method,
        bridge.add_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 5, 14, 15, plan.v2, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.mov(3, 15); // guarded Handler.v2 entry, kept through the packet-link guard
    emit_region_own_entry(a, layout, 6, 14, 15, bridge.packet_link, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);

    // Every remaining delivery guard runs before the prefix's first observable write. Success
    // commits the net prefix+delivery state directly; failure restores the native spill frame and
    // replays untouched pc59, preserving accessor/method/setter order without a staged fallback.
    let delivery_fail = emit_scheduler_handler_deliver_transaction(
        a,
        layout,
        &bridge.delivery,
        SchedulerHandlerDeliverSource::IncomingDevice,
        None,
        fast_resume,
        pc_labels,
    );
    a.bind(delivery_fail);
    a.b(fail);

    a.bind(packet_null);
    emit_scheduler_device_suspend(
        a,
        layout,
        &plan.suspend,
        None,
        false,
        fast_resume,
        fail,
        pc_labels,
    );
    fail
}

/// Lower the polymorphic task dispatch into the existing DeviceTask tails. No state is changed
/// until every property, method, descriptor, value-class, and HTMLDDA guard has succeeded; a
/// failure can therefore replay the original pc59 with an untouched stack and local set.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDevicePlan,
    prevalidated_entry: Option<usize>,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let plain_h = a.new_label();
    let packet_null = a.new_label();
    let commit_suspend = a.new_label();
    let commit_queue = a.new_label();
    let ev = layout.entry_value as i32;

    // The ordinary entry reloads and proves the complete receiver chain from frame locals.
    a.ldrb_imm(9, 22, plan.tcb_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(0, 22, plan.tcb_off + 8);
    emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, plain_h);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, plain_h);
    a.lsl_imm(5, 13, 16);
    a.lsr_imm(5, 5, 16);

    // Device shape and own v1 descriptor discriminate this virtual arm. Re-read the immediate
    // prototype's exact run method so replacing a value without changing shapes still deopts.
    emit_region_own_entry(a, layout, 5, 6, 7, plan.v1, false, plain_h);
    a.ldur(12, 7, ev); // packed v1, kept until branch classification
    a.ldr_imm(8, 6, layout.obj_proto as u32);
    a.cbz(8, true, plain_h);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.run_method,
        plan.run_expected,
        plain_h,
    );

    // The epoch selector enters only the compact v1 prefix. Branch around it from the ordinary
    // path, then converge before packet classification and every transactional tail.
    if let Some(prevalidated_entry) = prevalidated_entry {
        let body = a.new_label();
        a.b(body);
        a.bind(prevalidated_entry);
        emit_region_own_entry_trusted_shape(a, layout, 5, 6, 7, plan.v1, false, plain_h);
        a.ldur(12, 7, ev);
        a.bind(body);
    }

    // DeviceTask's loose-null test has an HTMLDDA exception. Exact Null is the hot null arm;
    // accept an object only when its ordinary/plain marker proves it cannot be HTMLDDA.
    a.ldrb_imm(9, 22, plan.packet_off);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_EQ, packet_null);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(13, 22, plan.packet_off + 8);
    a.add_imm(14, 13, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
    a.cbz(9, false, plain_h);
    if let Some(hold) = &plan.hold {
        emit_scheduler_device_hold(
            a,
            layout,
            hold,
            7,
            12,
            13,
            fast_resume,
            plain_h,
            pc_labels,
        );
    } else {
        emit_scheduler_device_commit(
            a,
            layout,
            plan,
            5,
            pc_labels[plan.hold_pc],
            fast_resume.is_some(),
        );
    }

    a.bind(packet_null);
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, commit_suspend);
    a.lsr_imm(9, 12, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, plain_h);
    a.lsl_imm(13, 12, 16);
    a.lsr_imm(13, 13, 16);
    a.add_imm(14, 13, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
    a.cbz(9, false, plain_h);
    a.b(commit_queue);

    a.bind(commit_suspend);
    if let Some(suspend) = &plan.suspend {
        emit_scheduler_device_suspend(
            a,
            layout,
            suspend,
            None,
            false,
            fast_resume,
            plain_h,
            pc_labels,
        );
    } else {
        emit_scheduler_device_commit(
            a,
            layout,
            plan,
            5,
            pc_labels[plan.suspend_pc],
            fast_resume.is_some(),
        );
    }
    a.bind(commit_queue);
    if let Some(queue) = &plan.queue {
        emit_scheduler_device_queue(
            a,
            layout,
            queue,
            7,
            13,
            SchedulerQueueSource::Clear,
            true,
            false,
            fast_resume,
            plain_h,
            pc_labels,
        );
    } else {
        emit_scheduler_device_commit(
            a,
            layout,
            plan,
            5,
            pc_labels[plan.queue_pc],
            fast_resume.is_some(),
        );
    }
    plain_h
}

/// The common SchedulerActive null-packet dispatcher proves the task receiver once, then routes
/// directly to exactly one role arm. A selected arm never cascades into another role after a late
/// value guard: it replays through the shared pc59 fallback instead. Shapes alone are insufficient
/// because canonical HandlerTask and WorkerTask have the same own fields, so every matching shape
/// is resolved by its exact live `run` method in the original Device/Handler/Idle/Worker order.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_active_null_role_dispatch_compatible(
    dispatch: &SchedulerActiveNullDispatchPlan,
) -> bool {
    dispatch.device.v1.recv_shape == dispatch.device.run_method.recv_shape
        && dispatch.handler.v1.recv_shape == dispatch.handler.run_method.recv_shape
        && dispatch.idle.as_ref().is_none_or(|idle| {
            let shape = idle.run_method.recv_shape;
            idle.release.count.recv_shape == shape
                && idle.release.v1.recv_shape == shape
                && idle.release.scheduler.recv_shape == shape
        })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_role_epoch_compatible(plan: &SchedulerShellPlan) -> bool {
    let Some(active) = &plan.active else {
        return false;
    };
    let Some(dispatch) = &active.null_dispatch else {
        return false;
    };
    active.run_method.recv_shape == active.state.recv_shape
        && dispatch.device.task.recv_shape == active.state.recv_shape
        && scheduler_active_null_role_dispatch_compatible(dispatch)
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_role_epoch_enabled(plan: &SchedulerShellPlan) -> bool {
    std::env::var_os("LUMEN_JIT_NO_SCHED_ROLE_DISPATCH").is_none()
        && std::env::var_os("LUMEN_JIT_NO_SCHED_ROLE_EPOCH").is_none()
        && scheduler_role_epoch_compatible(plan)
}

/// The graph epoch is deliberately narrower than the general Scheduler specialization. It
/// requires all six canonical roles and only own data ICs, but never keys on benchmark names or
/// compile-time object addresses. Runtime validation discovers and pins the exact six TCB/task
/// identities for one bounded session.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_graph_epoch_compatible(
    plan: &SchedulerShellPlan,
    layout: &crate::value::JitLayout,
) -> bool {
    let Some(active) = &plan.active else {
        return false;
    };
    let Some(dispatch) = &active.null_dispatch else {
        return false;
    };
    let (Some(idle), Some(worker), Some(queue)) = (
        dispatch.idle.as_ref(),
        dispatch.worker.as_ref(),
        dispatch.device.queue.as_ref(),
    ) else {
        return false;
    };
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    let own = |state: crate::bytecode::IcState| state.depth == 0;
    let tcb_shape = plan.state.recv_shape;
    let slots = [
        active.state.slot,
        active.queue.slot,
        plan.link.slot,
        active.id.slot,
        dispatch.device.task.slot,
    ];
    // These slots are encoded directly by the compact fill. Fail closed in release builds rather
    // than relying on assembler debug assertions whose immediates would otherwise truncate.
    let direct_slots_fit = layout.entry_accessor == layout.entry_writable
        && slots.into_iter().all(|slot| {
            let Some(off) = slot.checked_mul(layout.entry_size as u32) else {
                return false;
            };
            let Some(meta) = off.checked_add(layout.entry_accessor as u32) else {
                return false;
            };
            let value = off as i64 + layout.entry_value as i64;
            off < 4096 && meta < 4096 && (-256..=255).contains(&value)
        })
        && slots
            .into_iter()
            .max()
            .is_some_and(|slot| slot < 4095);
    direct_slots_fit
        && own(plan.state)
        && own(plan.link)
        && own(active.id)
        && own(active.state)
        && own(active.queue)
        && own(active.current_id)
        && own(queue.blocks)
        && own(dispatch.device.task)
        && same_ic(plan.state, active.state)
        && [
            plan.link.recv_shape,
            active.id.recv_shape,
            active.queue.recv_shape,
            dispatch.device.task.recv_shape,
        ]
        .into_iter()
        .all(|shape| shape == tcb_shape)
        && same_ic(dispatch.device.task, dispatch.handler.task)
        && same_ic(dispatch.device.task, idle.task)
        && same_ic(dispatch.device.task, worker.task)
        && [
            dispatch.device.run_method,
            dispatch.handler.run_method,
            idle.run_method,
            worker.run_method,
        ]
        .into_iter()
        .all(|method| method.depth == 1)
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_graph_epoch_enabled(
    plan: &SchedulerShellPlan,
    layout: &crate::value::JitLayout,
) -> bool {
    scheduler_role_epoch_enabled(plan)
        && std::env::var_os("LUMEN_JIT_NO_SCHED_GRAPH_EPOCH").is_none()
        && scheduler_graph_epoch_compatible(plan, layout)
}

/// The first graph-core contract proves the immutable task-to-Scheduler edge for every exact
/// role record. Suspend can then reuse the graph's state entry, pinned Scheduler/current, methods,
/// and state global without re-walking three object property chains on every null task step.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_graph_core_compatible(plan: &SchedulerShellPlan) -> bool {
    let Some(dispatch) = plan
        .active
        .as_ref()
        .and_then(|active| active.null_dispatch.as_ref())
    else {
        return false;
    };
    let (Some(idle), Some(worker), Some(device_suspend)) = (
        dispatch.idle.as_ref(),
        dispatch.worker.as_ref(),
        dispatch.device.suspend.as_ref(),
    ) else {
        return false;
    };
    [
        (idle.release.scheduler, idle.run_method.recv_shape),
        (worker.suspend.scheduler, worker.run_method.recv_shape),
        (
            dispatch.handler.suspend.scheduler,
            dispatch.handler.run_method.recv_shape,
        ),
        (
            device_suspend.scheduler,
            dispatch.device.run_method.recv_shape,
        ),
    ]
    .into_iter()
    .all(|(scheduler, task_shape)| scheduler.depth == 0 && scheduler.recv_shape == task_shape)
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_graph_core_enabled(plan: &SchedulerShellPlan) -> bool {
    std::env::var_os("LUMEN_JIT_NO_SCHED_GRAPH_CORE").is_none()
        && scheduler_method_epoch_enabled(plan)
        && scheduler_graph_core_compatible(plan)
}

/// Keep the second CORE consumer independently removable while its first benchmark slice is
/// evaluated. The base all-six task-to-Scheduler proof and null-suspend tail remain published.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_graph_core_incoming_enabled(plan: &SchedulerShellPlan) -> bool {
    std::env::var_os("LUMEN_JIT_NO_SCHED_GRAPH_CORE_INCOMING").is_none()
        && plan
            .active
            .as_ref()
            .and_then(|active| active.null_dispatch.as_ref())
            .is_some_and(|dispatch| dispatch.handler_incoming_suspend)
}

/// Map a live TCB Rc pointer to its exact stack record. `sp_bias` accounts for a nested native
/// spill. The current record is tried first because suspend and non-preempting queue keep it hot;
/// the full six-way scan remains the defensive remap after every direct continuation.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_find_record(
    a: &mut asm::Asm,
    tcb: u32,
    current_record: Option<u32>,
    out_record: u32,
    out_index: Option<u32>,
    sp_bias: u32,
    fail: usize,
) {
    let done = a.new_label();
    let mut matches = Vec::with_capacity(SCHED_GRAPH_RECORD_COUNT as usize);
    if let Some(current) = current_record {
        let scan = a.new_label();
        a.ldr_imm(9, current, SCHED_GRAPH_TCB_OFF);
        a.cmp_reg_x(tcb, 9);
        a.b_cond(C_NE, scan);
        a.mov(out_record, current);
        if out_index.is_none() {
            a.b(done);
        }
        // Consumers needing the role/index still take the full table scan. The shell state/link
        // consumer does not, making the overwhelmingly common same-current resume one compare.
        a.bind(scan);
    }
    for index in 0..SCHED_GRAPH_RECORD_COUNT {
        let matched = a.new_label();
        matches.push(matched);
        a.ldr_imm(
            9,
            31,
            scheduler_graph_record_sp(index) + sp_bias + SCHED_GRAPH_TCB_OFF,
        );
        a.cmp_reg_x(tcb, 9);
        a.b_cond(C_EQ, matched);
    }
    a.b(fail);
    for (index, matched) in matches.into_iter().enumerate() {
        a.bind(matched);
        a.add_imm(
            out_record,
            31,
            scheduler_graph_record_sp(index as u32) + sp_bias,
        );
        if let Some(out_index) = out_index {
            a.movz(out_index, index as u32, 0);
        }
        a.b(done);
    }
    a.bind(done);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_task_role_guard(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    task: u32,
    method: crate::bytecode::IcState,
    expected: usize,
    proto_slot: u32,
    fail: usize,
) {
    let rcv = layout.obj_from_rc as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;
    a.add_imm(6, task, rcv);
    a.ldrb_imm(9, 6, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 6, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, 6, shape);
    a.mov_imm64(10, method.recv_shape as u64);
    a.cmp_reg_w(9, 10);
    a.b_cond(C_NE, fail);
    a.ldr_imm(7, 6, layout.obj_proto as u32);
    a.cbz(7, true, fail);
    emit_region_proto_method(a, layout, 7, 14, 15, method, expected, fail);
    a.str_imm(7, 31, proto_slot);
}

/// The second Handler/Device task only needs to reach the exact role prototype whose run method
/// was proved by its first sibling earlier in the same no-user-code fill.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_cached_task_role_guard(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    task: u32,
    method: crate::bytecode::IcState,
    proto_slot: u32,
    fail: usize,
) {
    let shape = (layout.obj_props + layout.props_shape) as u32;
    a.add_imm(6, task, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 6, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 6, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, 6, shape);
    a.mov_imm64(10, method.recv_shape as u64);
    a.cmp_reg_w(9, 10);
    a.b_cond(C_NE, fail);
    a.ldr_imm(7, 6, layout.obj_proto as u32);
    a.ldr_imm(9, 31, proto_slot);
    a.cmp_reg_x(7, 9);
    a.b_cond(C_NE, fail);
}

/// Guard one already-addressed property metadata byte. Shape identity pins the slot/key; this
/// compact check retains the live accessor/writable semantics without repeating receiver/vector
/// discovery for every field.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_entry_meta_guard(
    a: &mut asm::Asm,
    entries: u32,
    meta_off: u32,
    writable: bool,
    fail: usize,
) {
    if writable {
        a.ldrb_imm(9, entries, meta_off);
        let mask = asm::logical_imm_w(
            (crate::value::PROP_ACCESSOR | crate::value::PROP_WRITABLE) as u32,
        )
        .unwrap();
        a.logic_imm_w(0, 9, 9, mask);
        a.cmp_imm_w(9, crate::value::PROP_WRITABLE as u32);
        a.b_cond(C_NE, fail);
    } else {
        guard_prop_data(a, 9, entries, meta_off, fail);
    }
}

/// Eagerly validate and populate the complete six-TCB graph while x28 is zero. The runtime loop
/// keeps static code compact; its cost is amortized across the following 1024 direct steps. No Rc
/// is cloned, no helper is called, and no heap pointer is retained outside the current frame.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_epoch_fill(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerShellPlan,
    scheduler: u32,
    current_entry: u32,
    tcb_proto: u32,
    fail: usize,
) {
    let active = plan.active.as_ref().expect("graph epoch requires Active");
    let dispatch = active
        .null_dispatch
        .as_ref()
        .expect("graph epoch requires role dispatch");
    let idle = dispatch.idle.as_ref().expect("graph epoch requires Idle");
    let worker = dispatch
        .worker
        .as_ref()
        .expect("graph epoch requires Worker");
    let queue = dispatch
        .device
        .queue
        .as_ref()
        .expect("graph epoch requires queue");
    let ev = layout.entry_value as i32;
    let elems = (layout.obj_props + layout.props_elems) as u32;

    // Invalid/partial cache words are unreachable until the final x28 publication.
    a.movz(28, 0, 0);
    a.stp_off(31, 31, SCHED_ROLE_DEVICE_PROTO_SP as i32);
    a.stp_off(31, 31, SCHED_ROLE_IDLE_PROTO_SP as i32);
    a.str_imm(5, 31, SCHED_GRAPH_HELD_SP);
    a.str_imm(6, 31, SCHED_GRAPH_SUSPENDED_SP);

    emit_region_own_entry(a, layout, scheduler, 3, 4, active.current_id, true, fail);
    a.str_imm(4, 31, SCHED_GRAPH_CURRENT_ID_ENTRY_SP);
    emit_region_packed_scalar(a, 4, ev, 13, fail);

    emit_region_own_entry(a, layout, scheduler, 3, 4, queue.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(7, 13, 16);
    a.lsr_imm(7, 7, 16);
    // The blocks pointer is needed only during this fill. Reuse its formerly write-only header
    // word for optional soft contracts, initially invalid until their complete proof publishes.
    a.str_imm(31, 31, SCHED_GRAPH_CORE_FLAGS_SP);

    // Exact six-element packed array. Holding the array through Scheduler.blocks keeps every
    // element owner and its entry vector stable for the helper-free session.
    a.add_imm(14, 7, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 14, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_imm(15, 14, elems);
    a.cbz(15, true, fail);
    a.ldr_imm(15, 15, layout.dense_packed as u32);
    a.cbz(15, true, fail);
    a.ldr_imm(16, 15, layout.vec_len_off as u32);
    a.cmp_imm_x(16, SCHED_GRAPH_RECORD_COUNT);
    a.b_cond(C_NE, fail);
    a.ldr_imm(11, 15, layout.vec_ptr_off as u32);

    let entry_size = layout.entry_size as u32;
    let state_off = active.state.slot * entry_size;
    let queue_off = active.queue.slot * entry_size;
    let link_off = plan.link.slot * entry_size;
    let id_off = active.id.slot * entry_size;
    let task_off = dispatch.device.task.slot * entry_size;
    let max_slot = [
        active.state.slot,
        active.queue.slot,
        plan.link.slot,
        active.id.slot,
        dispatch.device.task.slot,
    ]
    .into_iter()
    .max()
    .expect("five graph fields");
    debug_assert!(
        [state_off, queue_off, link_off, id_off, task_off]
            .into_iter()
            .all(|off| off + (layout.entry_accessor as u32) < 4096
                && off + (layout.entry_writable as u32) < 4096
                && (off as i32 + ev) < 256),
        "graph TCB entries must fit direct AArch64 offsets"
    );
    debug_assert_eq!(layout.entry_accessor, layout.entry_writable);

    // Discover exact identities and fixed entries. One shape/prototype/vector proof per TCB
    // makes its five known own slots directly addressable; each descriptor byte is still guarded
    // live once. Canonical links are resolved inline to the immediately preceding record.
    let fill_loop = a.new_label();
    let role_idle = a.new_label();
    let role_worker = a.new_label();
    let role_handler_first = a.new_label();
    let role_handler_cached = a.new_label();
    let role_device_first = a.new_label();
    let role_device_cached = a.new_label();
    let role_done = a.new_label();
    let link_previous = a.new_label();
    let link_done = a.new_label();
    a.movz(25, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.mov_imm64(23, crate::value::PACK_NULL);
    a.mov_imm64(24, crate::value::PACK_OBJ);
    a.mov_imm64(27, active.state.recv_shape as u64);
    a.add_imm(12, 31, SCHED_GRAPH_RECORDS_SP);
    a.movz(17, 0, 0);
    a.bind(fill_loop);
    a.add_shifted(15, 11, 17, 4);
    guard_prop_data(a, 9, 15, layout.property_meta as u32, fail);
    a.ldur(13, 15, layout.property_value as i32);
    a.lsr_imm(9, 13, 48);
    a.cmp_reg_x(9, 25);
    a.b_cond(C_NE, fail);
    a.lsl_imm(0, 13, 16);
    a.lsr_imm(0, 0, 16);
    a.str_imm(0, 12, SCHED_GRAPH_TCB_OFF);

    // The shell already pinned the exact common TCB prototype; every cached receiver must be an
    // ordinary object of the profiled common shape and reach that same prototype.
    a.add_imm(3, 0, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 3, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 3, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, 3, (layout.obj_props + layout.props_shape) as u32);
    a.cmp_reg_w(9, 27);
    a.b_cond(C_NE, fail);
    a.ldr_imm(9, 3, layout.obj_proto as u32);
    a.cmp_reg_x(9, tcb_proto);
    a.b_cond(C_NE, fail);
    a.ldr_imm(16, 3, (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32);
    a.cmp_imm_x(16, max_slot + 1);
    a.b_cond(C_LO, fail);
    a.ldr_imm(4, 3, (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32);

    emit_scheduler_graph_entry_meta_guard(
        a,
        4,
        state_off + layout.entry_writable as u32,
        true,
        fail,
    );
    a.add_imm(6, 4, state_off);
    a.str_imm(6, 12, SCHED_GRAPH_STATE_ENTRY_OFF);
    a.ldur(13, 4, state_off as i32 + ev);
    a.fmov_d_x(0, 13);
    emit_region_exact_i32(a, 0, 6, fail);

    emit_scheduler_graph_entry_meta_guard(
        a,
        4,
        queue_off + layout.entry_writable as u32,
        true,
        fail,
    );
    a.add_imm(6, 4, queue_off);
    a.str_imm(6, 12, SCHED_GRAPH_QUEUE_ENTRY_OFF);

    emit_scheduler_graph_entry_meta_guard(
        a,
        4,
        link_off + layout.entry_accessor as u32,
        false,
        fail,
    );
    a.ldur(13, 4, link_off as i32 + ev);
    a.cbnz(17, false, link_previous);
    a.cmp_reg_x(13, 23);
    a.b_cond(C_NE, fail);
    a.str_imm(31, 12, SCHED_GRAPH_LINK_RECORD_OFF);
    a.b(link_done);
    a.bind(link_previous);
    a.sub_imm(10, 12, SCHED_GRAPH_RECORD_SIZE);
    a.ldr_imm(9, 10, SCHED_GRAPH_TCB_OFF);
    a.logic_x(1, 7, 24, 9);
    a.cmp_reg_x(13, 7);
    a.b_cond(C_NE, fail);
    a.str_imm(10, 12, SCHED_GRAPH_LINK_RECORD_OFF);
    a.bind(link_done);

    emit_scheduler_graph_entry_meta_guard(
        a,
        4,
        id_off + layout.entry_accessor as u32,
        false,
        fail,
    );
    a.scvtf_d_w(0, 17);
    a.fmov_x_d(10, 0);
    a.ldur(13, 4, id_off as i32 + ev);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.str_imm(13, 12, SCHED_GRAPH_ID_BITS_OFF);

    emit_scheduler_graph_entry_meta_guard(
        a,
        4,
        task_off + layout.entry_accessor as u32,
        false,
        fail,
    );
    a.ldur(13, 4, task_off as i32 + ev);
    a.lsr_imm(9, 13, 48);
    a.cmp_reg_x(9, 25);
    a.b_cond(C_NE, fail);
    a.lsl_imm(5, 13, 16);
    a.lsr_imm(5, 5, 16);
    a.str_imm(5, 12, SCHED_GRAPH_TASK_OFF);

    // Role is structural and tied to the exact id/blocks slot, never to a name or baked object.
    a.cbz(17, true, role_idle);
    a.cmp_imm_w(17, 1);
    a.b_cond(C_EQ, role_worker);
    a.cmp_imm_w(17, 2);
    a.b_cond(C_EQ, role_handler_first);
    a.cmp_imm_w(17, 3);
    a.b_cond(C_EQ, role_handler_cached);
    a.cmp_imm_w(17, 4);
    a.b_cond(C_EQ, role_device_first);
    a.b(role_device_cached);

    a.bind(role_idle);
    emit_scheduler_graph_task_role_guard(
        a,
        layout,
        5,
        idle.run_method,
        idle.run_expected,
        SCHED_ROLE_IDLE_PROTO_SP,
        fail,
    );
    a.b(role_done);
    a.bind(role_worker);
    emit_scheduler_graph_task_role_guard(
        a,
        layout,
        5,
        worker.run_method,
        worker.run_expected,
        SCHED_ROLE_WORKER_PROTO_SP,
        fail,
    );
    a.b(role_done);
    a.bind(role_handler_first);
    emit_scheduler_graph_task_role_guard(
        a,
        layout,
        5,
        dispatch.handler.run_method,
        dispatch.handler.run_expected,
        SCHED_ROLE_HANDLER_PROTO_SP,
        fail,
    );
    a.b(role_done);
    a.bind(role_handler_cached);
    emit_scheduler_graph_cached_task_role_guard(
        a,
        layout,
        5,
        dispatch.handler.run_method,
        SCHED_ROLE_HANDLER_PROTO_SP,
        fail,
    );
    a.b(role_done);
    a.bind(role_device_first);
    emit_scheduler_graph_task_role_guard(
        a,
        layout,
        5,
        dispatch.device.run_method,
        dispatch.device.run_expected,
        SCHED_ROLE_DEVICE_PROTO_SP,
        fail,
    );
    a.b(role_done);
    a.bind(role_device_cached);
    emit_scheduler_graph_cached_task_role_guard(
        a,
        layout,
        5,
        dispatch.device.run_method,
        SCHED_ROLE_DEVICE_PROTO_SP,
        fail,
    );
    a.bind(role_done);
    a.add_imm(12, 12, SCHED_GRAPH_RECORD_SIZE);
    a.add_imm(17, 17, 1);
    a.cmp_imm_w(17, SCHED_GRAPH_RECORD_COUNT);
    a.b_cond(C_LO, fill_loop);

    // Distinct immediate role prototypes make cross-role task aliasing impossible while still
    // allowing the two Handler records or two Device records to share one task identity.
    for (right, right_slot) in [
        SCHED_ROLE_DEVICE_PROTO_SP,
        SCHED_ROLE_HANDLER_PROTO_SP,
        SCHED_ROLE_IDLE_PROTO_SP,
        SCHED_ROLE_WORKER_PROTO_SP,
    ]
    .into_iter()
    .enumerate()
    .skip(1)
    {
        a.ldr_imm(10, 31, right_slot);
        for left_slot in [
            SCHED_ROLE_DEVICE_PROTO_SP,
            SCHED_ROLE_HANDLER_PROTO_SP,
            SCHED_ROLE_IDLE_PROTO_SP,
            SCHED_ROLE_WORKER_PROTO_SP,
        ]
        .into_iter()
        .take(right)
        {
            a.ldr_imm(9, 31, left_slot);
            a.cmp_reg_x(10, 9);
            a.b_cond(C_EQ, fail);
        }
    }

    // Reload Scheduler.current after all scratch use and require that it is one exact graph node.
    a.ldur(13, current_entry, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(0, 13, 16);
    a.lsr_imm(0, 0, 16);
    emit_scheduler_graph_find_record(a, 0, None, 26, Some(17), 0, fail);
    a.ldr_imm(5, 31, SCHED_GRAPH_HELD_SP);
    a.ldr_imm(6, 31, SCHED_GRAPH_SUSPENDED_SP);
}

/// Softly extend an already-complete graph epoch with one immutable edge contract: every exact
/// cached task must retain an own data `scheduler` field naming the outer Scheduler. A rejection
/// has no JS-visible effect and leaves the graph epoch usable with its ordinary guarded tails.
/// The existing write-only graph-header word carries the validity bit, so this adds no frame or
/// heap storage. The live current record is remapped after both outcomes before any cache is used.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_graph_core_fill(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerShellPlan,
    scheduler: u32,
    current_entry: u32,
    graph_fail: usize,
) {
    let dispatch = plan
        .active
        .as_ref()
        .and_then(|active| active.null_dispatch.as_ref())
        .expect("graph core requires role dispatch");
    let idle = dispatch.idle.as_ref().expect("graph core requires Idle");
    let worker = dispatch
        .worker
        .as_ref()
        .expect("graph core requires Worker");
    let device_suspend = dispatch
        .device
        .suspend
        .as_ref()
        .expect("graph core requires Device suspend");
    let ev = layout.entry_value as i32;
    let rejected = a.new_label();
    let remap = a.new_label();
    let scheduler_fields = [
        idle.release.scheduler,
        worker.suspend.scheduler,
        dispatch.handler.suspend.scheduler,
        dispatch.handler.suspend.scheduler,
        device_suspend.scheduler,
        device_suspend.scheduler,
    ];

    // Graph fill pinned each exact task identity/prototype. Recheck the role-specific own field
    // recipe and its live value without invoking accessors; all six must agree before publication.
    for (index, field) in scheduler_fields.into_iter().enumerate() {
        a.ldr_imm(
            7,
            31,
            scheduler_graph_record_sp(index as u32) + SCHED_GRAPH_TASK_OFF,
        );
        emit_region_own_entry(a, layout, 7, 3, 4, field, false, rejected);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, rejected);
        a.lsl_imm(10, 13, 16);
        a.lsr_imm(10, 10, 16);
        a.cmp_reg_x(10, scheduler);
        a.b_cond(C_NE, rejected);
    }
    a.movz(9, SCHED_GRAPH_CORE_VALID, 0);
    a.str_imm(9, 31, SCHED_GRAPH_CORE_FLAGS_SP);
    a.b(remap);

    // The flags word was cleared by graph fill. Partial pointer/value observations own nothing,
    // so a soft miss simply joins the normal graph session with CORE_VALID left clear.
    a.bind(rejected);
    a.bind(remap);

    // Proof scratch is deliberately unconstrained. Re-establish the exact current record and the
    // two graph-header state globals after either result before the caller publishes x28.
    a.ldur(13, current_entry, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, graph_fail);
    a.lsl_imm(0, 13, 16);
    a.lsr_imm(0, 0, 16);
    emit_scheduler_graph_find_record(a, 0, None, 26, None, 0, graph_fail);
    a.ldr_imm(5, 31, SCHED_GRAPH_HELD_SP);
    a.ldr_imm(6, 31, SCHED_GRAPH_SUSPENDED_SP);
}

/// The standalone pc59 plans are rebuilt from the same bytecode/caches as Active's stitched
/// dispatcher. Require their shared TCB.task recipe and role shapes to line up before an epoch
/// selector is allowed to enter either emitter with x0/x5 prevalidated.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_pc59_role_dispatch_compatible(
    device: &SchedulerDevicePlan,
    handler: &SchedulerHandlerSuspendPlan,
) -> bool {
    let same_ic = |left: crate::bytecode::IcState, right: crate::bytecode::IcState| {
        left.recv_shape == right.recv_shape
            && left.holder_shape == right.holder_shape
            && left.slot == right.slot
            && left.depth == right.depth
            && left.mid_ok == right.mid_ok
            && left.mid_shape == right.mid_shape
            && left.mid2_shape == right.mid2_shape
    };
    device.tcb_off == handler.tcb_off
        && device.packet_off == handler.packet_off
        && same_ic(device.task, handler.task)
        && device.v1.recv_shape == device.run_method.recv_shape
        && handler.v1.recv_shape == handler.run_method.recv_shape
}

/// Re-enter pc59's Device/Handler classifiers through the bounded Scheduler epoch. No frame or
/// heap owner is created: x0/x5 remain borrowed from the materialized locals/current graph, and
/// the existing four native-frame words cache only non-owning immediate-prototype pointers.
/// Every miss branches to the untouched Device->Handler chain before either child can commit.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_pc59_role_selector(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    device: &SchedulerDevicePlan,
    handler: &SchedulerHandlerSuspendPlan,
    device_target: usize,
    handler_target: usize,
    fail: usize,
) {
    let ev = layout.entry_value as i32;
    let rcv = layout.obj_from_rc as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;

    // x23/x24/x25 and the prototype cache are meaningful only inside the bounded direct session.
    a.cbz(28, true, fail);

    // The materialized local must still be the exact object owned by pinned Scheduler.current.
    a.ldrb_imm(9, 22, device.tcb_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(0, 22, device.tcb_off + 8);
    a.ldur(13, 24, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(12, 13, 16);
    a.lsr_imm(12, 12, 16);
    a.cmp_reg_x(0, 12);
    a.b_cond(C_NE, fail);

    // Session entry pinned this exact ordinary TCB shape/prototype and proved TCB.run. Recheck
    // identity here because pc59 is also reachable from ordinary x28==0 execution.
    a.add_imm(3, 0, rcv);
    a.ldrb_imm(9, 3, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 3, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(9, 3, shape);
    a.mov_imm64(16, device.task.recv_shape as u64);
    a.cmp_reg_w(9, 16);
    a.b_cond(C_NE, fail);
    a.ldr_imm(9, 3, layout.obj_proto as u32);
    a.cmp_reg_x(9, 25);
    a.b_cond(C_NE, fail);

    // Load TCB.task exactly once. The receiver shape is pinned, but bounds and the live data
    // descriptor remain guards so a stale/moved entry vector cannot reach a selected child.
    emit_region_own_entry_trusted_shape(a, layout, 0, 3, 4, device.task, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(5, 13, 16);
    a.lsr_imm(5, 5, 16);
    a.add_imm(6, 5, rcv);
    a.ldrb_imm(9, 6, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 6, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(17, 6, shape);
    a.ldr_imm(8, 6, layout.obj_proto as u32);
    a.cbz(8, true, fail);

    // Preserve the canonical Device-before-Handler ordering. A cold cache way proves the exact
    // live run method before publishing its non-owning prototype pointer; a hot way is valid only
    // while x28 proves that no user code has run since that proof.
    for (method, expected, target, proto_slot) in [
        (
            device.run_method,
            device.run_expected,
            device_target,
            SCHED_ROLE_DEVICE_PROTO_SP,
        ),
        (
            handler.run_method,
            handler.run_expected,
            handler_target,
            SCHED_ROLE_HANDLER_PROTO_SP,
        ),
    ] {
        let next = a.new_label();
        let uncached = a.new_label();
        a.mov_imm64(16, method.recv_shape as u64);
        a.cmp_reg_w(17, 16);
        a.b_cond(C_NE, next);
        a.ldr_imm(16, 31, proto_slot);
        a.cbz(16, true, uncached);
        a.cmp_reg_x(8, 16);
        a.b_cond(C_NE, next);
        a.b(target);
        a.bind(uncached);
        emit_region_proto_method(a, layout, 8, 14, 15, method, expected, next);
        a.str_imm(8, 31, proto_slot);
        a.b(target);
        a.bind(next);
    }
    a.b(fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_epoch_method_matches(
    left: crate::bytecode::IcState,
    left_expected: usize,
    right: crate::bytecode::IcState,
    right_expected: usize,
) -> bool {
    left.depth == 1
        && right.depth == 1
        && left.holder_shape == right.holder_shape
        && left.slot == right.slot
        && left_expected == right_expected
}

/// The bounded scheduler session executes no user code. Prove the nested suspend/queue method
/// family once at its entry, then reuse those identities only while x28 keeps that session live.
/// Handler and Worker suspend plans are compiled from separate call sites, so require their exact
/// prototype entries and function identities to match the Device plan before sharing the proof.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_method_epoch_compatible(plan: &SchedulerShellPlan) -> bool {
    let Some(dispatch) = plan
        .active
        .as_ref()
        .and_then(|active| active.null_dispatch.as_ref())
    else {
        return false;
    };
    let Some(device_suspend) = dispatch.device.suspend.as_ref() else {
        return false;
    };
    if dispatch.device.queue.is_none() {
        return false;
    }
    let handler_suspend = &dispatch.handler.suspend;
    if !scheduler_epoch_method_matches(
        device_suspend.suspend_method,
        device_suspend.suspend_expected,
        handler_suspend.suspend_method,
        handler_suspend.suspend_expected,
    ) || !scheduler_epoch_method_matches(
        device_suspend.mark_method,
        device_suspend.mark_expected,
        handler_suspend.mark_method,
        handler_suspend.mark_expected,
    ) {
        return false;
    }
    dispatch.worker.as_ref().is_none_or(|worker| {
        scheduler_epoch_method_matches(
            device_suspend.suspend_method,
            device_suspend.suspend_expected,
            worker.suspend.suspend_method,
            worker.suspend.suspend_expected,
        ) && scheduler_epoch_method_matches(
            device_suspend.mark_method,
            device_suspend.mark_expected,
            worker.suspend.mark_method,
            worker.suspend.mark_expected,
        )
    })
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn scheduler_method_epoch_enabled(plan: &SchedulerShellPlan) -> bool {
    scheduler_role_epoch_enabled(plan)
        && std::env::var_os("LUMEN_JIT_NO_SCHED_METHOD_EPOCH").is_none()
        && scheduler_method_epoch_compatible(plan)
}

/// Route Active's still-virtual non-null packet from the exact current graph record. The saved
/// x26 word at Active SP+24 is the record pointer captured before the 48-byte Active spill; the
/// graph records themselves remain in the original scheduler frame at SP+48. Record zero is Idle
/// and cannot consume a non-null packet, while an unknown pointer fails closed to materialization.
/// The fill already pinned each record's exact task identity/prototype/run method, so the selected
/// arm receives that prevalidated task in x5 and must never cascade into a different role.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_packet_role_selector(
    a: &mut asm::Asm,
    worker: usize,
    handler: usize,
    device: usize,
    fail: usize,
) {
    const ACTIVE_SPILL: u32 = 48;

    a.ldr_imm(13, 31, 24); // saved exact current graph record
    for (index, target) in [
        (1, worker),
        (2, handler),
        (3, handler),
        (4, device),
        (5, device),
    ] {
        let next = a.new_label();
        a.add_imm(
            9,
            31,
            ACTIVE_SPILL + scheduler_graph_record_sp(index),
        );
        a.cmp_reg_x(13, 9);
        a.b_cond(C_NE, next);
        // Dereference only after proving the non-owning pointer is inside this live frame.
        a.ldr_imm(5, 13, SCHED_GRAPH_TASK_OFF);
        a.b(target);
        a.bind(next);
    }
    a.b(fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_null_role_selector(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    dispatch: &SchedulerActiveNullDispatchPlan,
    device: usize,
    handler: usize,
    idle: Option<usize>,
    worker: Option<usize>,
    role_epoch: bool,
    graph_epoch: bool,
    fail: usize,
) {
    if graph_epoch {
        // The eager fill tied each exact record/id to one structural task role and pinned the
        // exact task identity/prototype/run method. x26 was remapped from live current at the
        // common resume (or advanced through an exact cached link) before Active was entered.
        let idle = idle.expect("graph epoch requires Idle target");
        let worker = worker.expect("graph epoch requires Worker target");
        a.ldr_imm(5, 26, SCHED_GRAPH_TASK_OFF);
        for (index, target) in [idle, worker, handler, handler, device, device]
            .into_iter()
            .enumerate()
        {
            let next = a.new_label();
            a.add_imm(9, 31, scheduler_graph_record_sp(index as u32));
            a.cmp_reg_x(26, 9);
            a.b_cond(C_NE, next);
            a.b(target);
            a.bind(next);
        }
        a.b(fail);
        return;
    }
    let ev = layout.entry_value as i32;
    let rcv = layout.obj_from_rc as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;

    // Every profiled TCB role uses the same own `task` slot. Prove the current receiver and load
    // the task once; a noncanonical TCB simply replays the untouched pc59 dispatch.
    if role_epoch {
        emit_region_own_entry_trusted_shape(
            a,
            layout,
            0,
            3,
            4,
            dispatch.device.task,
            false,
            fail,
        );
    } else {
        emit_region_own_entry(a, layout, 0, 3, 4, dispatch.device.task, false, fail);
    }
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(5, 13, 16);
    a.lsr_imm(5, 5, 16);
    a.add_imm(6, 5, rcv);
    a.ldrb_imm(9, 6, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 6, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(17, 6, shape);
    a.ldr_imm(8, 6, layout.obj_proto as u32);
    a.cbz(8, true, fail);

    let candidates = [
        Some((
            dispatch.device.run_method,
            dispatch.device.run_expected,
            device,
            SCHED_ROLE_DEVICE_PROTO_SP,
        )),
        Some((
            dispatch.handler.run_method,
            dispatch.handler.run_expected,
            handler,
            SCHED_ROLE_HANDLER_PROTO_SP,
        )),
        dispatch
            .idle
            .as_ref()
            .zip(idle)
            .map(|(plan, target)| {
                (
                    plan.run_method,
                    plan.run_expected,
                    target,
                    SCHED_ROLE_IDLE_PROTO_SP,
                )
            }),
        dispatch
            .worker
            .as_ref()
            .zip(worker)
            .map(|(plan, target)| {
                (
                    plan.run_method,
                    plan.run_expected,
                    target,
                    SCHED_ROLE_WORKER_PROTO_SP,
                )
            }),
    ];
    for (method, expected, target, proto_slot) in candidates.into_iter().flatten() {
        let next = a.new_label();
        a.mov_imm64(16, method.recv_shape as u64);
        a.cmp_reg_w(17, 16);
        a.b_cond(C_NE, next);
        if role_epoch {
            let uncached = a.new_label();
            a.ldr_imm(16, 31, proto_slot);
            a.cbz(16, true, uncached);
            a.cmp_reg_x(8, 16);
            a.b_cond(C_NE, next);
            a.b(target);
            a.bind(uncached);
            emit_region_proto_method(a, layout, 8, 14, 15, method, expected, next);
            a.str_imm(8, 31, proto_slot);
        } else {
            emit_region_proto_method(a, layout, 8, 14, 15, method, expected, next);
        }
        a.b(target);
        a.bind(next);
    }
    a.b(fail);
}

/// Classify an exact-Null SchedulerActive packet while the active TCB remains virtual in x0.
/// Only tails that preserve Scheduler.current are emitted: Device suspend and the empty,
/// non-preempting Device.v1 queue transaction. Every guard precedes its selected tail's commit.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_device_active_null_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerDevicePlan,
    task_prevalidated: bool,
    core: Option<SchedulerGraphCoreContext>,
    method_epoch: bool,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let commit_suspend = a.new_label();
    let commit_queue = a.new_label();
    let ev = layout.entry_value as i32;

    // x0 is the active TCB, rooted by the pinned writable Scheduler.current entry in x24. The
    // common role selector can enter with x5 already holding an exact ordinary task whose shape,
    // prototype, and run method were checked once.
    if !task_prevalidated {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
    }

    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 5, 6, 7, plan.v1, false, fail);
    } else {
        emit_region_own_entry(a, layout, 5, 6, 7, plan.v1, false, fail);
    }
    a.ldur(12, 7, ev);
    if !task_prevalidated {
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }

    a.mov_imm64(13, crate::value::PACK_NULL);
    a.cmp_reg_x(12, 13);
    a.b_cond(C_EQ, commit_suspend);
    a.lsr_imm(9, 12, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(13, 12, 16);
    a.lsr_imm(13, 13, 16);
    a.add_imm(14, 13, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.b(commit_queue);

    a.bind(commit_suspend);
    let Some(suspend) = &plan.suspend else {
        a.b(fail);
        a.bind(commit_queue);
        a.b(fail);
        return fail;
    };
    emit_scheduler_device_suspend(
        a,
        layout,
        suspend,
        core,
        method_epoch,
        fast_resume,
        fail,
        pc_labels,
    );

    a.bind(commit_queue);
    let Some(queue) = &plan.queue else {
        a.b(fail);
        return fail;
    };
    emit_scheduler_device_queue(
        a,
        layout,
        queue,
        7,
        13,
        SchedulerQueueSource::Clear,
        true,
        method_epoch,
        fast_resume,
        fail,
        pc_labels,
    );
    fail
}

/// Handler counterpart of [`emit_scheduler_device_active_null_region`]. The incoming packet is
/// exact Null by construction. Besides wait/suspend, an optional stitched plan consumes the two
/// remaining work-list arms without materializing the compiler locals first: v2 delivery and
/// completed-v1 queue. Every child transaction retains its own guard-before-commit boundary.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_active_null_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerHandlerSuspendPlan,
    task_prevalidated: bool,
    core: Option<SchedulerGraphCoreContext>,
    method_epoch: bool,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let suspend = a.new_label();
    let v1_object = a.new_label();
    let completion = a.new_label();
    let ev = layout.entry_value as i32;

    if !task_prevalidated {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
    }

    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 5, 6, 7, plan.v1, false, fail);
    } else {
        emit_region_own_entry(a, layout, 5, 6, 7, plan.v1, false, fail);
    }
    if !task_prevalidated {
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }

    a.ldur(13, 7, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, v1_object);
    a.b(suspend);

    a.bind(v1_object);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.mov(3, 7); // preserve the writable-source candidate for completed-v1 queue
    emit_region_own_entry(a, layout, 8, 14, 15, plan.packet_a1, false, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 2, fail);
    emit_region_name_i32(a, layout, plan.data_size_cache, 7, fail);
    a.cmp_reg_w(2, 7);
    a.b_cond(C_GE, completion);

    // Below DATA_SIZE, exact Null v2 is still the common wait state. An object instead enters
    // the shared numeric-delivery transaction with x2=count, x3=v2 entry, x5=Handler, x6=P.
    emit_region_own_entry(a, layout, 5, 14, 15, plan.v2, false, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_EQ, suspend);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(6, 13, 16);
    a.lsr_imm(6, 6, 16);
    guard_prop_writable(a, 9, 15, layout.entry_writable as u32, fail);
    a.mov(3, 15);
    let Some(full) = &plan.null_full else {
        a.b(fail);
        a.bind(completion);
        a.b(fail);
        a.bind(suspend);
        emit_scheduler_device_suspend(
            a,
            layout,
            &plan.suspend,
            core,
            method_epoch,
            fast_resume,
            fail,
            pc_labels,
        );
        return fail;
    };
    let delivery_fail = emit_scheduler_handler_deliver_transaction(
        a,
        layout,
        &full.delivery,
        SchedulerHandlerDeliverSource::ActiveNull,
        None,
        fast_resume,
        pc_labels,
    );
    a.bind(delivery_fail);
    a.b(fail);

    // The completed work packet is still owned by Handler.v1. Reuse DeviceQueue's transfer
    // mode so Handler.v1 receives P.link and target.queue receives P without count traffic.
    a.bind(completion);
    emit_scheduler_device_queue(
        a,
        layout,
        &full.queue.queue,
        3,
        8,
        SchedulerQueueSource::AdvanceToPacketLink,
        true,
        false,
        fast_resume,
        fail,
        pc_labels,
    );

    a.bind(suspend);
    emit_scheduler_device_suspend(
        a,
        layout,
        &plan.suspend,
        core,
        method_epoch,
        fast_resume,
        fail,
        pc_labels,
    );

    fail
}

/// WorkerTask's null packet is a pure bridge to `scheduler.suspendCurrent()`. The Active frame
/// already contains scalar sentinels, so success only commits the shared numeric state tail.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_worker_active_null_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerActiveWorkerPlan,
    task_prevalidated: bool,
    core: Option<SchedulerGraphCoreContext>,
    method_epoch: bool,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let ev = layout.entry_value as i32;

    if !task_prevalidated {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
        a.add_imm(6, 5, layout.obj_from_rc as u32);
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }
    emit_scheduler_device_suspend(
        a,
        layout,
        &plan.suspend,
        core,
        method_epoch,
        fast_resume,
        fail,
        pc_labels,
    );
    fail
}

/// Consume SchedulerActive's still-virtual incoming Handler packet through Packet.addTo and the
/// post-add suspend arm. The transaction accepts the complete canonical bounded pools: at most
/// one old WORK packet in Handler.v1, or at most two old DEVICE packets in Handler.v2. Every
/// observable guard precedes commit; failure restores the Active queue-commit register snapshot
/// so the existing Rust materializer can replay TaskControlBlock.run and HandlerTask.run.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_handler_active_incoming_suspend_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    active: &SchedulerActivePlan,
    plan: &SchedulerHandlerSuspendPlan,
    task_prevalidated: bool,
    core: bool,
    incoming_suspend: bool,
    work_delivery: bool,
    state_entry: u32,
    tcb_proto: u32,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let incoming = plan
        .incoming
        .as_ref()
        .expect("Active Handler incoming plan checked by caller");
    let outer_fail = a.new_label();
    let fail = a.new_label();
    let work = a.new_label();
    let device = a.new_label();
    let lists_done = a.new_label();
    let ev = layout.entry_value as i32;

    // Fixed non-owning spill; no generated call occurs in the transaction.
    //   0 C/S, 16 pending-state/successor, 32 id/currentId-entry,
    //   48 queue-entry/session-record, 64 P/old-P.link, 80 state-entry/TCB-proto,
    //   96 stale locals, 112 add-destination/P.link-entry, 128 Handler/H.v2 entry,
    //   144 bounded list nodes, 160 W.a1/final state, 176 WORK selector/delivery D.
    // The enclosing 48-byte Active spill begins at +192. Its exact graph record/epoch are saved
    // at +216/+232, while the scheduler-frame CORE flags/suspended value are at +376/+392.
    // These are all existing non-owning words; this consumer does not extend either frame.
    a.stp_pre(0, 1, -192);
    a.stp_off(11, 12, 16);
    a.stp_off(23, 24, 32);
    a.stp_off(25, 26, 48);
    a.stp_off(27, 28, 64);
    a.stp_off(state_entry, tcb_proto, 80);
    a.stp_off(31, 31, 96);
    a.stp_off(31, 31, 112);
    a.stp_off(31, 31, 128);
    a.stp_off(31, 31, 144);
    a.stp_off(31, 31, 160);
    a.stp_off(31, 31, 176);

    // C.task -> exact Handler and exact live Handler.run. A graph-record router supplies the exact
    // already-pinned task in x5; the ordinary sequential path retains the complete proof here.
    // Handler and Worker deliberately share an own shape in Richards, so non-graph selection still
    // uses the method identity as its role discriminator after Worker has had first refusal.
    a.ldur(0, 31, 0);
    if !task_prevalidated {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
    }
    a.stur(5, 31, 128);
    for off in [0i32, 8, 64] {
        a.ldur(9, 31, off);
        a.cmp_reg_x(5, 9);
        a.b_cond(C_EQ, fail);
    }
    a.ldur(27, 31, 64);
    for off in [0i32, 8] {
        a.ldur(9, 31, off);
        a.cmp_reg_x(27, 9);
        a.b_cond(C_EQ, fail);
    }
    if !task_prevalidated {
        a.add_imm(6, 5, layout.obj_from_rc as u32);
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }

    // Prove stale compiler locals can be normalized without a last-owner destructor. Preserve
    // their raw owners until commit; unlike Worker's older transaction, no frame store occurs on
    // a later miss.
    emit_scheduler_active_guard_old_locals(a, layout, active, 6, 7, fail);
    a.stp_off(6, 7, 96);

    // packet.kind is an own exact integer and KIND_WORK is the one live binding observed by the
    // source comparison. Remember the selected arm across the shared addTo/link guards.
    a.ldur(27, 31, 64);
    emit_region_own_entry(a, layout, 27, 14, 15, incoming.kind, false, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 4, fail);
    emit_region_name_i32(a, layout, incoming.kind_work_cache, 7, fail);
    let selected = a.new_label();
    a.cmp_reg_w(4, 7);
    let select_device = a.new_label();
    a.b_cond(C_NE, select_device);
    a.movz(17, 1, 0);
    a.b(selected);
    a.bind(select_device);
    a.movz(17, 0, 0);
    a.bind(selected);
    a.stur(17, 31, 176);

    // Reject the delivery outcomes before walking the addTo target list. DEVICE delivery has a
    // non-Null v1 and WORK delivery has a non-Null v2; these exact own-data checks keep those much
    // hotter existing transactions from paying the bounded-list guards of this suspend-only arm.
    let gate_work = a.new_label();
    let gate_done = a.new_label();
    a.cbnz(17, false, gate_work);
    a.ldur(5, 31, 128);
    emit_region_own_entry(a, layout, 5, 14, 15, plan.v1, false, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.b(gate_done);
    a.bind(gate_work);
    a.ldur(5, 31, 128);
    emit_region_own_entry(a, layout, 5, 14, 15, plan.v2, false, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    if work_delivery {
        // Null retains the existing WORK-suspend path. Only the former non-Null miss pays the
        // narrow delivery guards, so DEVICE and canonical wait outcomes gain no guard chain.
        a.b_cond(C_EQ, gate_done);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(6, 13, 16);
        a.lsr_imm(6, 6, 16);
        guard_prop_writable(a, 9, 15, layout.entry_writable as u32, fail);
        for off in [0i32, 8, 64, 128] {
            a.ldur(9, 31, off);
            a.cmp_reg_x(6, 9);
            a.b_cond(C_EQ, fail);
        }
        a.ldur(9, 31, 72);
        a.mov_imm64(10, crate::value::PACK_NULL);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.ldur(9, 31, 24);
        a.cbnz(9, false, fail);
        a.ldur(9, 31, 16);
        a.mov_imm64(10, active.running as u64);
        a.cmp_reg_w(9, 10);
        a.b_cond(C_NE, fail);
        a.stur(15, 31, 136);
        a.stur(6, 31, 184);
    } else {
        a.b_cond(C_NE, fail);
    }
    a.bind(gate_done);

    // Both structurally parsed arms call the same exact Packet.addTo. Re-resolve P.link as a
    // writable own data entry and require the exact packed word Active observed before any user
    // code; this entry is the source owner moved into C.queue at commit.
    a.ldur(27, 31, 64);
    a.add_imm(14, 27, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        incoming.add_method,
        incoming.add_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 27, 14, 15, incoming.packet_link, true, fail);
    a.stur(15, 31, 120);
    a.ldur(13, 15, ev);
    a.ldur(9, 31, 72);
    a.cmp_reg_x(13, 9);
    a.b_cond(C_NE, fail);

    a.ldur(17, 31, 176);
    a.cbnz(17, false, work);
    a.b(device);

    // WORK -> Handler.v1. Empty and exactly-one-node old lists cover the canonical two-packet
    // pool. In the one-node case addTo returns the unchanged head, so the later Handler.v1 store
    // is an exact data-property no-op and only head.link needs publication.
    a.bind(work);
    a.ldur(5, 31, 128);
    emit_region_own_entry(a, layout, 5, 3, 4, plan.v1, true, fail);
    a.ldur(13, 4, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let work_one = a.new_label();
    let work_list_ready = a.new_label();
    a.b_cond(C_NE, work_one);
    a.stur(4, 31, 112); // H.v1 receives P
    a.ldur(8, 31, 64); // effective W is incoming P
    a.b(work_list_ready);

    a.bind(work_one);
    if work_delivery {
        // The minimal delivery source only covers pre-add H.v1 Null, making virtual W exactly the
        // incoming packet. Existing one-node WORK suspend remains available when no D was saved.
        a.ldur(9, 31, 184);
        a.cbnz(9, false, fail);
    }
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.stur(8, 31, 144);
    for off in [0i32, 8, 64, 128] {
        a.ldur(9, 31, off);
        a.cmp_reg_x(8, 9);
        a.b_cond(C_EQ, fail);
    }
    emit_region_own_entry(a, layout, 8, 14, 15, incoming.packet_link, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.stur(15, 31, 112); // sole old tail.link receives P

    a.bind(work_list_ready);
    // Post-add Handler logic suspends only while effective W.a1 < DATA_SIZE and H.v2 is Null.
    emit_region_own_entry(a, layout, 8, 14, 15, plan.packet_a1, false, fail);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 2, fail);
    if work_delivery {
        let suspend_count = a.new_label();
        a.ldur(9, 31, 184);
        a.cbz(9, true, suspend_count);
        guard_prop_writable(a, 9, 15, layout.entry_writable as u32, fail);
        a.stur(15, 31, 160);
        a.cmp_imm_w(2, 0);
        a.b_cond(C_NE, fail);
        a.bind(suspend_count);
    }
    emit_region_name_i32(a, layout, plan.data_size_cache, 7, fail);
    a.cmp_reg_w(2, 7);
    a.b_cond(C_GE, fail);
    if work_delivery {
        let suspend_only = a.new_label();
        a.ldur(6, 31, 184);
        a.cbz(6, true, suspend_only);
        a.ldur(3, 31, 136);
        a.ldur(4, 31, 160);
        a.ldur(5, 31, 128);
        a.ldur(8, 31, 64);
        let delivery_fail = emit_scheduler_handler_deliver_transaction(
            a,
            layout,
            &incoming.delivery,
            SchedulerHandlerDeliverSource::ActiveIncomingWork,
            Some((active, state_entry, tcb_proto)),
            fast_resume,
            pc_labels,
        );
        a.bind(delivery_fail);
        a.b(fail);
        a.bind(suspend_only);
    }
    if !incoming_suspend {
        a.b(fail);
    }
    a.b(lists_done);

    // DEVICE -> Handler.v2. With three packets in each canonical Handler/Device pool, excluding
    // incoming P leaves at most two old nodes. All traversed link sites are exact own data slots;
    // only an exact Null terminal is accepted as the count-neutral publication destination.
    a.bind(device);
    if !incoming_suspend {
        a.b(fail);
    }
    a.ldur(5, 31, 128);
    emit_region_own_entry(a, layout, 5, 3, 4, plan.v2, true, fail);
    a.ldur(13, 4, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let device_head = a.new_label();
    let device_list_ready = a.new_label();
    a.b_cond(C_NE, device_head);
    a.stur(4, 31, 112); // H.v2 receives P
    a.b(device_list_ready);

    a.bind(device_head);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.stur(8, 31, 144);
    for off in [0i32, 8, 64, 128] {
        a.ldur(9, 31, off);
        a.cmp_reg_x(8, 9);
        a.b_cond(C_EQ, fail);
    }
    emit_region_own_entry(a, layout, 8, 14, 15, incoming.packet_link, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    let device_second = a.new_label();
    a.b_cond(C_NE, device_second);
    a.stur(15, 31, 112); // one-node tail.link receives P
    a.b(device_list_ready);

    a.bind(device_second);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.stur(8, 31, 152);
    for off in [0i32, 8, 64, 128, 144] {
        a.ldur(9, 31, off);
        a.cmp_reg_x(8, 9);
        a.b_cond(C_EQ, fail);
    }
    emit_region_own_entry(a, layout, 8, 14, 15, incoming.packet_link, true, fail);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.stur(15, 31, 112); // two-node tail.link receives P

    a.bind(device_list_ready);

    a.bind(lists_done);
    // Destination must still be the exact Null owner slot selected above. Reject every bounded
    // cycle/graph alias, including an Active successor that is also a Handler-list node.
    a.ldur(15, 31, 112);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    a.ldur(12, 31, 24);
    let successor_distinct = a.new_label();
    a.cbz(12, true, successor_distinct);
    for off in [0i32, 8, 64, 128, 144, 152] {
        let next = a.new_label();
        a.ldur(9, 31, off);
        a.cbz(9, true, next);
        a.cmp_reg_x(12, 9);
        a.b_cond(C_EQ, fail);
        a.bind(next);
    }
    a.bind(successor_distinct);

    let core_guards = core.then(|| (a.new_label(), a.new_label()));
    if let Some((guarded, ready)) = core_guards {
        // The outer Active snapshot, not this transaction's live x26/x28 pair, is authoritative:
        // tracing deliberately reuses live x26, and this nested spill saved those live values at
        // +56/+72. The exact graph record and published epoch remain in Active's +24/+40 words.
        a.ldr_imm(9, 31, 232);
        a.cbz(9, true, guarded);
        a.ldr_imm(9, 31, 376);
        a.movz(10, SCHED_GRAPH_CORE_VALID, 0);
        a.logic_w(0, 9, 9, 10);
        a.cbz(9, false, guarded);
        a.ldr_imm(10, 31, 216);
        a.cbz(10, true, guarded);
        a.ldr_imm(9, 10, SCHED_GRAPH_TCB_OFF);
        a.ldur(0, 31, 0);
        a.cmp_reg_x(9, 0);
        a.b_cond(C_NE, guarded);

        // Active already saved the exact graph state entry at +80. CORE contributes the stable
        // suspended integer; pending-state validation and every commit/owner operation stay
        // shared below rather than becoming part of this optional contract.
        a.ldr_imm(5, 31, 392);
        a.b(ready);
        a.bind(guarded);
    }

    // Exact Handler.scheduler === outer S, exact suspendCurrent/current assignment, and exact
    // markAsSuspended/state/global. C.state still contains old SUSPENDED_RUNNABLE here; compute
    // from Active's pending RUNNING/RUNNABLE value instead of reusing the ordinary suspend tail.
    a.ldur(5, 31, 128);
    emit_region_own_entry(a, layout, 5, 3, 4, plan.suspend.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldur(9, 31, 8);
    a.cmp_reg_x(1, 9);
    a.b_cond(C_NE, fail);
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(2, 14, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    a.add_imm(3, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.suspend.suspend_method,
        plan.suspend.suspend_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 1, 3, 4, plan.suspend.current, true, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(2, 13, 16);
    a.lsr_imm(2, 2, 16);
    a.ldur(0, 31, 0);
    a.cmp_reg_x(2, 0);
    a.b_cond(C_NE, fail);

    a.add_imm(3, 0, layout.obj_from_rc as u32);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    a.ldur(9, 31, 88);
    a.cmp_reg_x(8, 9);
    a.b_cond(C_NE, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        plan.suspend.mark_method,
        plan.suspend.mark_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 0, 3, 4, plan.suspend.state, true, fail);
    a.ldur(9, 31, 80);
    a.cmp_reg_x(4, 9);
    a.b_cond(C_NE, fail);
    emit_region_name_i32(a, layout, plan.suspend.suspended_cache, 5, fail);

    if let Some((_, ready)) = core_guards {
        a.bind(ready);
    }

    // The pending state must agree with the already-validated successor classification. This
    // both catches register drift and proves the collapsed TaskControlBlock.run + suspend store.
    a.ldur(6, 31, 16);
    a.ldur(12, 31, 24);
    let pending_running = a.new_label();
    let pending_ready = a.new_label();
    a.cbz(12, true, pending_running);
    a.mov_imm64(9, active.runnable as u64);
    a.cmp_reg_w(6, 9);
    a.b_cond(C_NE, fail);
    a.b(pending_ready);
    a.bind(pending_running);
    a.mov_imm64(9, active.running as u64);
    a.cmp_reg_w(6, 9);
    a.b_cond(C_NE, fail);
    a.bind(pending_ready);
    a.logic_w(1, 6, 6, 5);
    a.stur(6, 31, 168);

    // --- commit: no guard or call follows ---
    // Scalar scheduler/TCB effects can precede the ownership transfers. Publish P's destination
    // before replacing C.queue, then publish successor in C.queue before clearing P.link. The two
    // moves remain count-neutral even when P or successor has no other owner.
    a.ldur(23, 31, 32);
    a.ldur(24, 31, 40);
    a.stur(23, 24, ev);
    a.ldur(6, 31, 168);
    a.scvtf_d_w(0, 6);
    a.fmov_x_d(13, 0);
    a.ldur(4, 31, 80);
    a.stur(13, 4, ev);

    a.ldur(27, 31, 64);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 27);
    a.ldur(15, 31, 112);
    a.stur(13, 15, ev);
    a.ldur(13, 31, 72);
    a.ldur(25, 31, 48);
    a.stur(13, 25, ev);
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.ldur(15, 31, 120);
    a.stur(13, 15, ev);

    // Successful stitched iterations must not retain skipped compiler-local graphs. Both old
    // owner counts were proven non-last before commit, so these bare decrements cannot destruct.
    a.movz(9, 0, 0); // Undefined
    a.str_imm(9, 22, active.tcb_off);
    a.str_imm(31, 22, active.tcb_off + 8);
    a.movz(9, 2, 0); // Null
    a.str_imm(9, 22, active.packet_off);
    a.str_imm(31, 22, active.packet_off + 8);
    a.ldp_off(6, 7, 96);
    emit_scheduler_active_drop_old_locals(a, layout, 6, 7);

    // Discard the transaction spill, then the enclosing Active spill exactly once. Scheduler.current
    // is unchanged, so the restored graph record/session budget remains valid for fast_resume.
    a.ldp_off(11, 12, 16);
    a.ldp_off(23, 24, 32);
    a.ldp_off(25, 26, 48);
    a.ldp_off(27, 28, 64);
    a.ldp_off(state_entry, tcb_proto, 80);
    a.ldp_post(0, 1, 192);
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    emit_scheduler_loop_continue(a, fast_resume, plan.suspend.loop_pc, pc_labels);

    a.bind(fail);
    // Restore only this nested transaction. The surrounding Active snapshot and its virtual
    // C/P/queue state remain untouched for the generic materializer directly after queue_commit.
    a.ldp_off(11, 12, 16);
    a.ldp_off(23, 24, 32);
    a.ldp_off(25, 26, 48);
    a.ldp_off(27, 28, 64);
    a.ldp_off(state_entry, tcb_proto, 80);
    a.ldp_post(0, 1, 192);
    a.b(outer_fail);
    outer_fail
}

/// Execute WorkerTask's non-null packet arm, Scheduler.queue, and the empty/preempting
/// checkPriorityAdd arm as one guard-before-commit transaction. This is selected structurally
/// from the live call/profile graph: no Richards name or object identity is baked into the code.
/// A declined guard falls into Active's ordinary packet materialization, so accessors, coercions,
/// uncommon aliases, method replacements, and partial throws retain exact source behavior.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_worker_active_packet_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    active: &SchedulerActivePlan,
    plan: &SchedulerActiveWorkerPlan,
    task_prevalidated: bool,
    state_entry: u32,
    tcb_proto: u32,
    fast_resume: Option<usize>,
    pc_labels: &[usize],
) -> usize {
    let work = plan.work.as_ref().expect("packet work checked by caller");
    let queue = &work.queue;
    let outer_fail = a.new_label();
    let fail = a.new_label();
    let ev = layout.entry_value as i32;
    let pv = layout.property_value as i32;
    let strong = layout.rc_strong_off as i32;

    // Snapshot every Active value needed by the generic continuation. The remaining words hold
    // guarded entry pointers only; all storage is native-stack bounded and disappears on either
    // edge (there is no per-iteration heap allocation).
    //   0 C/S, 16 pending state/link, 32 id/currentId, 48 source/scratch,
    //   64 packet/link word, 80 state entry/TCB proto,
    //   96 Worker v1/v2, 112 packet id/a1, 128..152 payload entries,
    //   160 queueCount/packet.link, 176 current/target.queue,
    //   192 target.state/target, 208 stale locals, 224 selected id/a2 array.
    a.stp_pre(0, 1, -256);
    a.stp_off(11, 12, 16);
    a.stp_off(23, 24, 32);
    a.stp_off(25, 26, 48);
    a.stp_off(27, 28, 64);
    a.stp_off(state_entry, tcb_proto, 80);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 39, 0);
        a.stur(9, 31, 240);
    }

    // C.task -> exact WorkerTask and exact live WorkerTask.run. A graph-record router supplies the
    // exact already-pinned task in x5; the ordinary sequential path retains the complete proof.
    // Reject cross-role aliases in both cases: the direct stores below intentionally assume Worker,
    // packet, current TCB, and Scheduler are independent ordinary objects as in the profile.
    a.ldur(0, 31, 0);
    if task_prevalidated {
        a.mov(23, 5);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(23, 13, 16);
        a.lsr_imm(23, 23, 16);
    }
    a.ldur(24, 31, 64); // incoming packet P
    a.cmp_reg_x(23, 0);
    a.b_cond(C_EQ, fail);
    a.cmp_reg_x(23, 24);
    a.b_cond(C_EQ, fail);
    a.ldur(1, 31, 8);
    a.cmp_reg_x(23, 1);
    a.b_cond(C_EQ, fail);
    a.cmp_reg_x(24, 0);
    a.b_cond(C_EQ, fail);
    a.cmp_reg_x(24, 1);
    a.b_cond(C_EQ, fail);
    if !task_prevalidated {
        a.add_imm(6, 23, layout.obj_from_rc as u32);
        a.ldr_imm(8, 6, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 40, 0);
        a.stur(9, 31, 240);
    }

    // The skipped inline-frame locals are dead on success. Clear them before later guards so a
    // long trusted session cannot retain one stale graph per role; a later miss reconstructs the
    // ordinary (C,P) snapshot in Active's existing materializer.
    emit_scheduler_active_guard_old_locals(a, layout, active, 6, 7, fail);
    a.stp_off(6, 7, 208);
    a.movz(9, 0, 0); // Value::Undefined
    a.str_imm(9, 22, active.tcb_off);
    a.str_imm(31, 22, active.tcb_off + 8);
    a.movz(9, 2, 0); // Value::Null
    a.str_imm(9, 22, active.packet_off);
    a.str_imm(31, 22, active.packet_off + 8);
    emit_scheduler_active_drop_old_locals(a, layout, 6, 7);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 41, 0);
        a.stur(9, 31, 240);
    }

    // Toggle Worker.v1 using the three live NameIC sites. Restrict the old value to the two hot
    // exact integers; loose-equality coercions and changed bindings fall through before any JS
    // state is written.
    emit_region_own_entry(a, layout, 23, 3, 4, work.v1, true, fail);
    a.stur(4, 31, 96);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_exact_i32(a, 0, 5, fail);
    emit_region_name_i32(a, layout, work.id_a_cache, 6, fail);
    emit_region_name_i32(a, layout, work.id_a_else_cache, 8, fail);
    a.cmp_reg_w(6, 8);
    a.b_cond(C_NE, fail);
    // The NameIC decoder uses x7 as its packed/wide flag. Load ID_B last so that scratch use
    // cannot overwrite the value before the toggle comparison below.
    emit_region_name_i32(a, layout, work.id_b_cache, 7, fail);
    let old_is_a = a.new_label();
    let id_ready = a.new_label();
    a.cmp_reg_w(5, 6);
    a.b_cond(C_EQ, old_is_a);
    a.cmp_reg_w(5, 7);
    a.b_cond(C_NE, fail);
    a.mov(5, 6);
    a.b(id_ready);
    a.bind(old_is_a);
    a.mov(5, 7);
    a.bind(id_ready);
    a.stur(5, 31, 224);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 48, 0);
        a.stur(9, 31, 240);
    }

    emit_region_name_i32(a, layout, work.data_size_cache, 8, fail);
    a.cmp_imm_w(8, 4);
    a.b_cond(C_NE, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 49, 0);
        a.stur(9, 31, 240);
    }

    // Worker.v2 is an exact small integer in the profiled cycle. This excludes overflow and
    // coercion while still covering every canonical Richards iteration.
    emit_region_own_entry(a, layout, 23, 3, 4, work.v2, true, fail);
    a.stur(4, 31, 104);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_exact_i32(a, 0, 5, fail);
    a.cmp_imm_w(5, 0);
    a.b_cond(C_MI, fail);
    a.mov_imm64(6, work.threshold as u64);
    a.cmp_reg_w(5, 6);
    a.b_cond(C_GT, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 42, 0);
        a.stur(9, 31, 240);
    }

    // P.id, P.a1, and P.a2[0..4) must all be writable, already-number data slots. Replacing a
    // reference or invoking an indexed setter would require observable destruction/user code and
    // therefore uses the ordinary Worker loop instead.
    emit_region_own_entry(a, layout, 24, 3, 4, queue.packet_id, true, fail);
    a.stur(4, 31, 112);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_own_entry(a, layout, 24, 3, 4, work.packet_a1, true, fail);
    a.stur(4, 31, 120);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_own_entry(a, layout, 24, 3, 4, work.packet_a2, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.stur(8, 31, 232);
    for (index, spill) in [(0u32, 128i32), (1, 136), (2, 144), (3, 152)] {
        a.movz(6, index, 0);
        emit_scheduler_packed_entry(a, layout, 8, 6, 15, fail);
        guard_prop_writable(
            a,
            9,
            15,
            layout.property_meta as u32,
            fail,
        );
        emit_region_packed_number(a, 15, pv, 0, fail);
        a.stur(15, 31, spill);
    }
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 43, 0);
        a.stur(9, 31, 240);
    }

    // Worker.scheduler must be the outer schedule receiver. Pin the exact queue method before
    // any Worker effect, matching source evaluation while making the later nested calls removable.
    emit_region_own_entry(a, layout, 23, 3, 4, queue.scheduler, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.ldur(2, 31, 8);
    a.cmp_reg_x(1, 2);
    a.b_cond(C_NE, fail);
    a.add_imm(3, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        queue.queue_method,
        queue.queue_expected,
        fail,
    );
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 44, 0);
        a.stur(9, 31, 240);
    }

    // scheduler.blocks[new Worker.v1] -> target T. The payload array may not alias blocks because
    // source order fills it before queue() performs this lookup.
    emit_region_own_entry(a, layout, 1, 3, 4, queue.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(8, 13, 16);
    a.lsr_imm(8, 8, 16);
    a.ldur(9, 31, 232);
    a.cmp_reg_x(8, 9);
    a.b_cond(C_EQ, fail);
    a.ldur(6, 31, 224);
    emit_scheduler_packed_target(a, layout, 8, 6, 25, fail);
    a.stur(25, 31, 200);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 45, 0);
        a.stur(9, 31, 240);
    }

    emit_region_own_entry(a, layout, 1, 3, 4, queue.queue_count, true, fail);
    a.stur(4, 31, 160);
    emit_region_packed_number(a, 4, ev, 2, fail);
    a.fmov_one(0);
    a.f_arith(0, 2, 2, 0);

    // Revalidate P.link's writable entry and unchanged packed value. Active read it before any
    // user code and retained the exact source word at sp+72; object/Null/Undefined all move
    // correctly into C.queue at commit.
    emit_region_own_entry(a, layout, 24, 3, 4, queue.packet_link, true, fail);
    a.stur(4, 31, 168);
    a.ldur(13, 4, ev);
    a.ldur(9, 31, 72);
    a.cmp_reg_x(13, 9);
    a.b_cond(C_NE, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 46, 0);
        a.stur(9, 31, 240);
    }

    // scheduler.current must still own C and be an ordinary writable data entry. checkPriorityAdd
    // and markAsRunnable are both pinned before the first store.
    emit_region_own_entry(a, layout, 1, 3, 4, queue.current, true, fail);
    a.stur(4, 31, 176);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(26, 13, 16);
    a.lsr_imm(26, 26, 16);
    a.ldur(0, 31, 0);
    a.cmp_reg_x(26, 0);
    a.b_cond(C_NE, fail);
    a.cmp_reg_x(25, 26);
    a.b_cond(C_EQ, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 50, 0);
        a.stur(9, 31, 240);
    }

    a.add_imm(14, 25, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    a.ldur(9, 31, 88);
    a.cmp_reg_x(8, 9);
    a.b_cond(C_NE, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        queue.check_method,
        queue.check_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 25, 14, 15, queue.target_queue, true, fail);
    a.stur(15, 31, 184);
    a.ldur(13, 15, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 10);
    a.b_cond(C_NE, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 51, 0);
        a.stur(9, 31, 240);
    }

    a.add_imm(14, 25, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        queue.mark_method,
        queue.mark_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 25, 14, 15, queue.state, true, fail);
    a.stur(15, 31, 192);
    emit_region_packed_number(a, 15, ev, 0, fail);
    emit_region_exact_i32(a, 0, 6, fail);
    emit_region_name_i32(a, layout, queue.runnable_cache, 7, fail);
    a.logic_w(1, 28, 6, 7);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 52, 0);
        a.stur(9, 31, 240);
    }

    emit_region_own_entry(a, layout, 25, 14, 15, queue.target_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 6, fail);
    emit_region_own_entry(a, layout, 26, 14, 15, queue.current_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 7, fail);
    a.fcmp(6, 7);
    a.b_cond(C_LE, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 53, 0);
        a.stur(9, 31, 240);
    }
    a.ldur(9, 26, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.movz(9, 47, 0);
        a.stur(9, 31, 240);
    }

    // --- commit: all remaining operations are drop-free scalar stores or count-neutral owner
    // moves. Publish destination owners before removing their sources; no JS can observe the
    // transient duplicate because every potentially callable operation was guarded above. ---
    a.ldur(23, 31, 32); // packed C.id -> Scheduler.currentId and final P.id
    a.ldur(24, 31, 40);
    a.stur(23, 24, ev);
    a.ldur(4, 31, 80);
    a.ldur(11, 31, 16);
    a.scvtf_d_w(0, 11);
    a.fmov_x_d(13, 0);
    a.stur(13, 4, ev);

    a.ldur(5, 31, 224);
    a.scvtf_d_w(0, 5);
    a.fmov_x_d(13, 0);
    a.ldur(4, 31, 96);
    a.stur(13, 4, ev); // Worker.v1
    a.ldur(4, 31, 120);
    a.stur(31, 4, ev); // P.a1 = +0

    // Preserve the source loop's v2-before-element order, although all five destinations are
    // proven numeric data slots and therefore cannot run code or release an owner.
    a.ldur(4, 31, 104);
    emit_region_packed_number(a, 4, ev, 0, fail);
    emit_region_exact_i32(a, 0, 5, fail);
    for spill in [128i32, 136, 144, 152] {
        let in_range = a.new_label();
        a.add_imm(5, 5, 1);
        a.mov_imm64(6, work.threshold as u64);
        a.cmp_reg_w(5, 6);
        a.b_cond(C_LE, in_range);
        a.mov_imm64(5, work.reset as u64);
        a.bind(in_range);
        a.scvtf_d_w(0, 5);
        a.fmov_x_d(13, 0);
        a.stur(13, 4, ev); // Worker.v2++ / optional reset
        a.ldur(15, 31, spill);
        a.stur(13, 15, pv); // P.a2[i]
    }
    a.ldur(4, 31, 112);
    a.stur(23, 4, ev); // queue()'s final P.id = Scheduler.currentId
    a.ldur(4, 31, 160);
    a.fmov_x_d(13, 2);
    a.stur(13, 4, ev); // ++queueCount

    // Move P: C.queue -> target.queue, and old P.link: P.link -> C.queue. Destination-first
    // publication makes both transfers count-neutral even when either source is the last owner.
    a.ldur(24, 31, 64);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 24);
    a.ldur(4, 31, 184);
    a.stur(13, 4, ev);
    a.ldur(13, 31, 72);
    a.ldur(4, 31, 48);
    a.stur(13, 4, ev);
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.ldur(4, 31, 168);
    a.stur(13, 4, ev);

    a.scvtf_d_w(0, 28);
    a.fmov_x_d(13, 0);
    a.ldur(4, 31, 192);
    a.stur(13, 4, ev);

    // Outer schedule assignment: retain T before replacing the only current-entry owner, then
    // release C. The >1 guard above keeps generated code out of a last-owner destructor.
    emit_region_clone_rc(a, 25, strong);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 25);
    a.ldur(4, 31, 176);
    a.stur(13, 4, ev);
    a.ldur(0, 31, 0);
    a.ldur(9, 0, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);

    // Discard the transaction spill, restore the enclosing Active spill (the trusted-session
    // registers), and continue directly with the newly selected target TCB.
    a.ldp_post(0, 1, 256);
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    emit_scheduler_loop_continue(a, fast_resume, queue.loop_pc, pc_labels);

    a.bind(fail);
    if std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some() {
        a.ldur(0, 31, 240);
        a.mov_imm64(16, jit_scheduler_trace_fail as *const () as usize as u64);
        a.blr(16);
    }
    a.ldp_off(11, 12, 16);
    a.ldp_off(23, 24, 32);
    a.ldp_off(25, 26, 48);
    a.ldp_off(27, 28, 64);
    a.ldp_off(state_entry, tcb_proto, 80);
    a.ldp_post(0, 1, 256);
    a.b(outer_fail);
    outer_fail
}

/// Execute IdleTask's hot release arm directly from SchedulerActive. The active TCB is saved on
/// the native stack while x0 becomes the Idle receiver; success publishes the returned target to
/// the pinned outer Scheduler.current entry and resumes the scheduler without a JIT call/return.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_idle_active_null_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerActiveIdlePlan,
    task_prevalidated: bool,
    fast_resume: Option<usize>,
    loop_pc: usize,
    pc_labels: &[usize],
) -> usize {
    let fail = a.new_label();
    let outer_fail = a.new_label();
    let idle = &plan.release;
    let ev = layout.entry_value as i32;
    let strong = layout.rc_strong_off as i32;

    // Preserve the virtual TCB and outer Scheduler across all child guards. No generated call is
    // made in this transaction, so a balanced 16-byte pair is the complete side snapshot.
    a.stp_pre(0, 1, -16);
    if !task_prevalidated {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.task, false, fail);
        a.ldur(13, 4, ev);
        a.lsr_imm(9, 13, 48);
        a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.lsl_imm(5, 13, 16);
        a.lsr_imm(5, 5, 16);
        a.add_imm(14, 5, layout.obj_from_rc as u32);
        a.ldr_imm(8, 14, layout.obj_proto as u32);
        a.cbz(8, true, fail);
        emit_region_proto_method(
            a,
            layout,
            8,
            14,
            15,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }
    a.mov(0, 5); // exact IdleTask receiver for the child transaction

    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 0, 3, 5, idle.count, true, fail);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 5, idle.count, true, fail);
    }
    emit_region_packed_number(a, 5, ev, 0, fail);
    emit_region_exact_i32(a, 0, 17, fail);
    let hot_count = a.new_label();
    a.cmp_imm_x(17, 1);
    a.b_cond(C_GT, hot_count);
    a.b(fail);
    a.bind(hot_count);
    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 0, 3, 6, idle.v1, true, fail);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 6, idle.v1, true, fail);
    }
    emit_region_packed_number(a, 6, ev, 2, fail);
    emit_region_exact_i32(a, 2, 17, fail);

    // This specialization requires Idle.scheduler to be the outer scheduler whose epoch lives
    // in x23/x24. A changed scheduler object simply falls back before the first write.
    if task_prevalidated {
        emit_region_own_entry_trusted_shape(a, layout, 0, 3, 4, idle.scheduler, false, fail);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 4, idle.scheduler, false, fail);
    }
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(1, 13, 16);
    a.lsr_imm(1, 1, 16);
    a.cmp_reg_x(1, 23);
    a.b_cond(C_NE, fail);
    a.add_imm(14, 1, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        idle.release_method,
        idle.release_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 1, 3, 4, idle.blocks, false, fail);
    a.ldur(13, 4, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(2, 13, 16);
    a.lsr_imm(2, 2, 16);

    let odd = a.new_label();
    let selected = a.new_label();
    a.movz(9, 1, 0);
    a.logic_w(0, 9, 17, 9);
    a.cbnz(9, false, odd);
    a.asr_imm_w(17, 17, 1);
    a.scvtf_d_w(2, 17);
    emit_region_name_i32(a, layout, idle.id_a_cache, 7, fail);
    a.b(selected);
    a.bind(odd);
    a.asr_imm_w(17, 17, 1);
    a.movz(9, 0xD008, 0);
    a.logic_w(2, 17, 17, 9);
    a.scvtf_d_w(2, 17);
    emit_region_name_i32(a, layout, idle.id_b_cache, 7, fail);
    a.bind(selected);
    emit_scheduler_packed_target(a, layout, 2, 7, 3, fail);

    a.add_imm(14, 3, layout.obj_from_rc as u32);
    a.ldr_imm(8, 14, layout.obj_proto as u32);
    a.cbz(8, true, fail);
    if fast_resume.is_some() {
        // The release preempts to a different TCB. A trusted continuation may reuse the pinned
        // TaskControlBlock.run fact only when the target has that exact common prototype.
        a.cmp_reg_x(8, 25);
        a.b_cond(C_NE, fail);
    }
    emit_region_proto_method(
        a,
        layout,
        8,
        14,
        15,
        idle.mark_method,
        idle.mark_expected,
        fail,
    );
    emit_region_own_entry(a, layout, 3, 14, 8, idle.state, true, fail);

    emit_region_own_entry(a, layout, 1, 14, 15, idle.current, true, fail);
    a.cmp_reg_x(15, 24);
    a.b_cond(C_NE, fail);
    a.ldur(13, 15, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(4, 13, 16);
    a.lsr_imm(4, 4, 16);
    // The source release method can only replace current with a distinct, higher-priority TCB.
    // Rejecting self-replacement explicitly keeps the generated ownership transfer conservative
    // even if a user mutates both priority fields between profile collection and this trace.
    a.cmp_reg_x(3, 4);
    a.b_cond(C_EQ, fail);
    a.ldur(9, 31, 0); // saved active TCB
    a.cmp_reg_x(4, 9);
    a.b_cond(C_NE, fail);
    a.ldur(9, 4, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, fail);
    emit_region_own_entry(a, layout, 3, 14, 15, idle.target_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 4, fail);
    emit_region_own_entry(a, layout, 4, 14, 15, idle.current_priority, false, fail);
    emit_region_packed_number(a, 15, ev, 5, fail);
    let precommit = a.new_label();
    a.fcmp(4, 5);
    a.b_cond(C_GT, precommit);
    a.b(fail);
    a.bind(precommit);

    emit_region_name_i32(a, layout, idle.not_held_cache, 7, fail);
    emit_region_packed_number(a, 8, ev, 0, fail);
    emit_region_exact_i32(a, 0, 17, fail);
    a.logic_w(0, 17, 17, 7);

    // --- commit in source order, followed by the outer scheduler assignment ---
    emit_region_packed_number(a, 5, ev, 0, fail);
    emit_region_exact_i32(a, 0, 7, fail);
    a.sub_imm(7, 7, 1);
    a.scvtf_d_w(0, 7);
    a.fmov_x_d(13, 0);
    a.stur(13, 5, ev);
    a.fmov_x_d(13, 2);
    a.stur(13, 6, ev);
    a.scvtf_d_w(0, 17);
    a.fmov_x_d(13, 0);
    a.stur(13, 8, ev);

    emit_region_clone_rc(a, 3, strong);
    a.mov_imm64(13, crate::value::PACK_OBJ);
    a.logic_x(1, 13, 13, 3);
    a.stur(13, 24, ev);
    a.ldur(0, 31, 0);
    a.ldur(9, 0, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);
    a.ldp_post(0, 1, 16);
    emit_scheduler_loop_continue(a, fast_resume, loop_pc, pc_labels);

    a.bind(fail);
    a.ldp_post(0, 1, 16);
    a.b(outer_fail);
    outer_fail
}

/// Lower pcs 30..58 of the Richards-style scheduler transaction. Every check precedes the first
/// write; failure replays at pc30. Success owns the exact TCB/packet locals and resumes at the
/// polymorphic task dispatch with an empty operand stack.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerActivePlan,
    state_entry: u32,
    state_value: u32,
    tcb_proto: u32,
    active_pc: usize,
    fast_resume: Option<usize>,
    trust_names: bool,
    role_epoch: bool,
    method_epoch: bool,
    graph_epoch: bool,
    graph_core: bool,
    graph_core_incoming: bool,
    plain_dispatch: Option<usize>,
    pc_labels: &[usize],
) {
    let fail = a.new_label();
    let no_packet = a.new_label();
    let link_nullish = a.new_label();
    let queue_commit = a.new_label();
    let no_link_clone = a.new_label();
    let strong = layout.rc_strong_off as i32;
    let ev = layout.entry_value as i32;
    let trace = std::env::var_os("LUMEN_JIT_SCHED_TRACE").is_some();
    let inline_null_materialize =
        std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_INLINE_NULL").is_none();

    // x23=id bits, x24=currentId entry, x25=queue entry, x27=packet,
    // x28=state until the branch and then packed packet.link. These registers are ABI-owned by
    // our caller, so bracket the region and restore them on both success and replay.
    a.stp_pre(23, 24, -48);
    a.stp_off(25, 26, 16);
    a.stp_off(27, 28, 32);
    a.mov(28, state_value);

    if graph_epoch {
        // The common shell remap made x26 the exact current record. Its id is immutable and the
        // header's currentId entry was proven writable/non-owning before x28 publication.
        a.ldr_imm(23, 26, SCHED_GRAPH_ID_BITS_OFF);
        a.ldr_imm(
            24,
            31,
            SCHED_GRAPH_CURRENT_ID_ENTRY_SP + 48, // Active's x23..x28 spill
        );
    } else {
        if trace {
            a.movz(26, 1, 0);
        }
        emit_region_own_entry(a, layout, 0, 3, 10, plan.id, false, fail);
        emit_region_packed_number(a, 10, ev, 0, fail);
        a.ldur(23, 10, ev);
        if trace {
            a.movz(26, 2, 0);
        }
        emit_region_own_entry(a, layout, 1, 11, 24, plan.current_id, true, fail);
        emit_region_packed_scalar(a, 24, ev, 12, fail);
    }
    if trace {
        a.movz(26, 3, 0);
    }
    if !role_epoch {
        emit_region_proto_method(
            a,
            layout,
            tcb_proto,
            11,
            12,
            plan.run_method,
            plan.run_expected,
            fail,
        );
    }
    if trace {
        a.movz(26, 4, 0);
    }
    if trust_names {
        a.mov_imm64(5, plan.suspended_runnable as u64);
    } else {
        emit_region_name_i32(
            a,
            layout,
            plan.suspended_runnable_cache,
            5,
            fail,
        );
    }
    a.cmp_reg_w(28, 5);
    a.b_cond(C_NE, no_packet);

    // Suspended+runnable: packet=this.queue; this.queue=packet.link; state reflects whether the
    // new queue is nullish. All three globals and all descriptors remain live guards.
    if trace {
        a.movz(26, 5, 0);
    }
    if trust_names {
        a.mov_imm64(6, plan.running as u64);
        a.mov_imm64(7, plan.runnable as u64);
    } else {
        emit_region_name_i32(a, layout, plan.running_cache, 6, fail);
        emit_region_name_i32(a, layout, plan.runnable_cache, 7, fail);
    }
    guard_prop_writable(
        a,
        9,
        state_entry,
        layout.entry_writable as u32,
        fail,
    );
    if trace {
        a.movz(26, 6, 0);
    }
    if graph_epoch {
        // x26 may be trace scratch inside this spill; its saved word at sp+24 is the exact record.
        a.ldr_imm(11, 31, 24);
        a.ldr_imm(25, 11, SCHED_GRAPH_QUEUE_ENTRY_OFF);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 25, plan.queue, true, fail);
    }
    a.ldur(13, 25, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(27, 13, 16);
    a.lsr_imm(27, 27, 16);
    if trace {
        a.movz(26, 7, 0);
    }
    emit_region_own_entry(a, layout, 27, 11, 12, plan.packet_link, false, fail);
    a.ldur(28, 12, ev);
    a.mov_imm64(10, crate::value::PACK_NULL);
    a.cmp_reg_x(28, 10);
    a.b_cond(C_EQ, link_nullish);
    a.mov_imm64(10, crate::value::PACK_UNDEFINED);
    a.cmp_reg_x(28, 10);
    a.b_cond(C_EQ, link_nullish);
    if trace {
        a.movz(26, 8, 0);
    }
    a.lsr_imm(9, 28, 48);
    a.movz(10, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 10);
    a.b_cond(C_NE, fail);
    a.lsl_imm(12, 28, 16);
    a.lsr_imm(12, 12, 16); // linked packet owner, zero means nullish below
    a.add_imm(10, 12, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 10, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(9, 10, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail); // includes HTMLDDA, which is loosely equal to null
    a.mov(11, 7); // STATE_RUNNABLE
    a.b(queue_commit);

    a.bind(link_nullish);
    a.movz(12, 0, 0);
    a.mov(11, 6); // STATE_RUNNING

    a.bind(queue_commit);
    let packet_role_dispatch = graph_epoch
        && std::env::var_os("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH").is_none();
    if packet_role_dispatch {
        let worker_role = a.new_label();
        let handler_role = a.new_label();
        let device_role = a.new_label();
        let generic_packet = a.new_label();
        emit_scheduler_active_packet_role_selector(
            a,
            worker_role,
            handler_role,
            device_role,
            generic_packet,
        );

        a.bind(worker_role);
        if let Some(worker) = plan
            .null_dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.worker.as_ref())
            .filter(|worker| worker.work.is_some())
        {
            let worker_fail = emit_scheduler_worker_active_packet_region(
                a,
                layout,
                plan,
                worker,
                true,
                state_entry,
                tcb_proto,
                fast_resume,
                pc_labels,
            );
            a.bind(worker_fail);
        }
        a.b(generic_packet);

        a.bind(handler_role);
        if let Some(dispatch) = plan
            .null_dispatch
            .as_ref()
            .filter(|dispatch| {
                dispatch.handler_incoming_suspend || dispatch.handler_incoming_work_delivery
            })
        {
            let handler_fail = emit_scheduler_handler_active_incoming_suspend_region(
                a,
                layout,
                plan,
                &dispatch.handler,
                true,
                graph_core_incoming,
                dispatch.handler_incoming_suspend,
                dispatch.handler_incoming_work_delivery,
                state_entry,
                tcb_proto,
                fast_resume,
                pc_labels,
            );
            a.bind(handler_fail);
        }
        a.b(generic_packet);

        // Device packet records remain deliberately classified so they cannot cascade through
        // Worker or Handler. The generic materializer retains the faster complete Device path.
        a.bind(device_role);
        a.b(generic_packet);

        a.bind(generic_packet);
    } else {
        // Keep the non-graph/kill-switch path in its original Worker-then-Handler order. A late
        // Worker decline may still reach Handler here, matching the byte sequence predating the
        // graph-record router.
        if let Some(worker) = plan
            .null_dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.worker.as_ref())
            .filter(|worker| worker.work.is_some())
        {
            let worker_fail = emit_scheduler_worker_active_packet_region(
                a,
                layout,
                plan,
                worker,
                false,
                state_entry,
                tcb_proto,
                fast_resume,
                pc_labels,
            );
            a.bind(worker_fail);
        }
        if let Some(dispatch) = plan
            .null_dispatch
            .as_ref()
            .filter(|dispatch| {
                dispatch.handler_incoming_suspend || dispatch.handler_incoming_work_delivery
            })
        {
            let handler_fail = emit_scheduler_handler_active_incoming_suspend_region(
                a,
                layout,
                plan,
                &dispatch.handler,
                false,
                false,
                dispatch.handler_incoming_suspend,
                dispatch.handler_incoming_work_delivery,
                state_entry,
                tcb_proto,
                fast_resume,
                pc_labels,
            );
            a.bind(handler_fail);
        }
    }
    // The new queue needs its owner before the old packet owner is released. The safe Rust
    // materializer below clones C/P and performs any last-owner stale-local destructors.
    a.cbz(12, true, no_link_clone);
    emit_region_clone_rc(a, 12, strong);
    a.bind(no_link_clone);
    a.stur(23, 24, ev);
    a.scvtf_d_w(0, 11);
    a.fmov_x_d(13, 0);
    a.stur(13, state_entry, ev);
    a.add_imm(2, 0, layout.gc_data_off as u32);
    a.add_imm(3, 27, layout.gc_data_off as u32);
    a.add_imm(0, 22, plan.tcb_off);
    a.add_imm(1, 22, plan.packet_off);
    a.mov_imm64(16, jit_scheduler_materialize as *const () as usize as u64);
    a.blr(16);
    a.stur(28, 25, ev);
    // Slot2 now owns the packet, so replacing the queue's former packet owner cannot destroy it.
    a.ldur(9, 27, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 27, strong);
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    a.b(pc_labels[plan.exit_pc]);

    a.bind(no_packet);
    a.stur(23, 24, ev);
    if let (Some(dispatch), Some(plain_dispatch)) = (&plan.null_dispatch, plain_dispatch) {
        let clear_locals = a.new_label();
        let sentinels_ready = a.new_label();
        let slow_materialize = a.new_label();

        // Repeated stitched iterations leave `(Undefined, Null)` in the two virtual slots. That
        // sentinel needs no stores or count traffic; otherwise clear the old roots exactly before
        // entering task guards so successful tails retain no hidden graph.
        a.ldrb_imm(9, 22, plan.tcb_off);
        a.cbz(9, false, clear_locals);
        a.ldrb_imm(9, 22, plan.packet_off);
        a.cmp_imm_w(9, 2);
        a.b_cond(C_EQ, sentinels_ready);
        a.bind(clear_locals);
        emit_scheduler_active_guard_old_locals(a, layout, plan, 25, 27, slow_materialize);
        a.movz(9, 0, 0); // Value::Undefined
        a.str_imm(9, 22, plan.tcb_off);
        a.str_imm(31, 22, plan.tcb_off + 8);
        a.movz(9, 2, 0); // Value::Null
        a.str_imm(9, 22, plan.packet_off);
        a.str_imm(31, 22, plan.packet_off + 8);
        emit_scheduler_active_drop_old_locals(a, layout, 25, 27);

        a.bind(sentinels_ready);
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 48);
        let role_dispatch = std::env::var_os("LUMEN_JIT_NO_SCHED_ROLE_DISPATCH").is_none()
            && scheduler_active_null_role_dispatch_compatible(dispatch);
        if role_dispatch {
            // The Active snapshot has been fully restored: x26 is again the exact graph record
            // and SP is the scheduler-frame base. Non-graph role dispatch keeps the guarded path.
            let core = graph_core.then_some(SchedulerGraphCoreContext {
                current_record: 26,
                sp_bias: 0,
            });
            let role_fail = a.new_label();
            let device_role = a.new_label();
            let handler_role = a.new_label();
            let idle_role = dispatch.idle.as_ref().map(|_| a.new_label());
            let worker_role = dispatch.worker.as_ref().map(|_| a.new_label());
            emit_scheduler_active_null_role_selector(
                a,
                layout,
                dispatch,
                device_role,
                handler_role,
                idle_role,
                worker_role,
                role_epoch,
                graph_epoch,
                role_fail,
            );

            a.bind(device_role);
            let device_fail = emit_scheduler_device_active_null_region(
                a,
                layout,
                &dispatch.device,
                true,
                core,
                method_epoch,
                fast_resume,
                pc_labels,
            );
            a.bind(device_fail);
            a.b(role_fail);

            a.bind(handler_role);
            let handler_fail = emit_scheduler_handler_active_null_region(
                a,
                layout,
                &dispatch.handler,
                true,
                core,
                method_epoch,
                fast_resume,
                pc_labels,
            );
            a.bind(handler_fail);
            a.b(role_fail);

            if let (Some(idle), Some(idle_role)) = (&dispatch.idle, idle_role) {
                a.bind(idle_role);
                let idle_fail = emit_scheduler_idle_active_null_region(
                    a,
                    layout,
                    idle,
                    true,
                    fast_resume,
                    dispatch.handler.suspend.loop_pc,
                    pc_labels,
                );
                a.bind(idle_fail);
                a.b(role_fail);
            }
            if let (Some(worker), Some(worker_role)) = (&dispatch.worker, worker_role) {
                a.bind(worker_role);
                let worker_fail = emit_scheduler_worker_active_null_region(
                    a,
                    layout,
                    worker,
                    true,
                    core,
                    method_epoch,
                    fast_resume,
                    pc_labels,
                );
                a.bind(worker_fail);
                a.b(role_fail);
            }
            a.bind(role_fail);
        } else {
            let device_fail = emit_scheduler_device_active_null_region(
                a,
                layout,
                &dispatch.device,
                false,
                None,
                method_epoch,
                fast_resume,
                pc_labels,
            );
            a.bind(device_fail);
            // The Device classifier owns x0 only by convention; reload from the pinned current
            // entry before the independent Handler arm so future scratch changes cannot couple
            // the two.
            a.ldur(13, 24, ev);
            a.lsl_imm(0, 13, 16);
            a.lsr_imm(0, 0, 16);
            let handler_fail = emit_scheduler_handler_active_null_region(
                a,
                layout,
                &dispatch.handler,
                false,
                None,
                method_epoch,
                fast_resume,
                pc_labels,
            );
            a.bind(handler_fail);
            if let Some(idle) = &dispatch.idle {
                a.ldur(13, 24, ev);
                a.lsl_imm(0, 13, 16);
                a.lsr_imm(0, 0, 16);
                let idle_fail = emit_scheduler_idle_active_null_region(
                    a,
                    layout,
                    idle,
                    false,
                    fast_resume,
                    dispatch.handler.suspend.loop_pc,
                    pc_labels,
                );
                a.bind(idle_fail);
            }
            if let Some(worker) = &dispatch.worker {
                a.ldur(13, 24, ev);
                a.lsl_imm(0, 13, 16);
                a.lsr_imm(0, 0, 16);
                let worker_fail = emit_scheduler_worker_active_null_region(
                    a,
                    layout,
                    worker,
                    false,
                    None,
                    method_epoch,
                    fast_resume,
                    pc_labels,
                );
                a.bind(worker_fail);
            }
        }

        // All classifiers declined without mutation. Reconstruct the ordinary pc59 frame from
        // the still-pinned Scheduler.current entry, then jump after the duplicate canonical
        // classifiers. The scalar sentinels own nothing, so this is one clone and two wide stores.
        a.ldur(13, 24, ev);
        a.lsl_imm(0, 13, 16);
        a.lsr_imm(0, 0, 16);
        emit_region_clone_rc(a, 0, strong);
        a.movz(9, 8, 0); // Value::Obj
        a.str_imm(9, 22, plan.tcb_off);
        a.str_imm(0, 22, plan.tcb_off + 8);
        a.movz(9, 2, 0); // Value::Null
        a.str_imm(9, 22, plan.packet_off);
        a.str_imm(31, 22, plan.packet_off + 8);
        a.movz(28, 0, 0);
        a.b(plain_dispatch);

        // A rare BigInt/last-owner stale local needs Rust destruction. Materialize the complete
        // snapshot while the Active spill is still live, then use the canonical pc59 classifiers.
        a.bind(slow_materialize);
        a.add_imm(2, 0, layout.gc_data_off as u32);
        a.movz(3, 0, 0);
        a.add_imm(0, 22, plan.tcb_off);
        a.add_imm(1, 22, plan.packet_off);
        a.mov_imm64(16, jit_scheduler_materialize as *const () as usize as u64);
        a.blr(16);
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 48);
        a.b(pc_labels[plan.exit_pc]);
    } else if inline_null_materialize {
        let slow_materialize = a.new_label();
        let materialized = a.new_label();
        emit_scheduler_active_materialize_null_inline(
            a,
            layout,
            plan,
            slow_materialize,
        );
        a.b(materialized);

        // BigInt, a last shared reference, or two stale locals whose common allocation lacks a
        // third owner need Rust's full destructor path. All semantic guards have succeeded and
        // currentId is already committed, so this is an exact continuation rather than a replay.
        a.bind(slow_materialize);
        a.add_imm(2, 0, layout.gc_data_off as u32);
        a.movz(3, 0, 0);
        a.add_imm(0, 22, plan.tcb_off);
        a.add_imm(1, 22, plan.packet_off);
        a.mov_imm64(16, jit_scheduler_materialize as *const () as usize as u64);
        a.blr(16);
        a.bind(materialized);
    } else {
        a.add_imm(2, 0, layout.gc_data_off as u32);
        a.movz(3, 0, 0);
        a.add_imm(0, 22, plan.tcb_off);
        a.add_imm(1, 22, plan.packet_off);
        a.mov_imm64(16, jit_scheduler_materialize as *const () as usize as u64);
        a.blr(16);
    }
    if plan.null_dispatch.is_none() || plain_dispatch.is_none() {
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 48);
        a.b(pc_labels[plan.exit_pc]);
    }

    a.bind(fail);
    if trace {
        a.mov(0, 26);
        a.mov_imm64(16, jit_scheduler_trace_fail as *const () as usize as u64);
        a.blr(16);
    }
    a.ldp_off(25, 26, 16);
    a.ldp_off(27, 28, 32);
    a.ldp_post(23, 24, 48);
    if fast_resume.is_some() {
        a.movz(28, 0, 0);
    }
    a.b(pc_labels[active_pc]);
}

/// Replace SchedulerActive's TCB/packet locals with `(current, Null)` without crossing the Rust
/// ABI. The two new values are constructed before either stale owner is released. Values that
/// could require a destructor (BigInt, a last reference, or a twice-aliased allocation without a
/// surviving owner) branch to `slow`, which performs the exact `Value` replacements in Rust.
///
/// On the inline path x25/x27 hold the old reference payloads (zero for scalar locals). The
/// surrounding active-region spill owns both registers, and x0 remains the borrowed current TCB.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_materialize_null_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerActivePlan,
    slow: usize,
) {
    let strong = layout.rc_strong_off as i32;
    emit_scheduler_active_guard_old_locals(a, layout, plan, 25, 27, slow);
    // Create the new frame owner first, then publish both complete wide Values before releasing
    // either old owner. This preserves source/old aliases and keeps the frame exactly rooted.
    emit_region_clone_rc(a, 0, strong);
    a.movz(9, 8, 0); // Value::Obj
    a.str_imm(9, 22, plan.tcb_off);
    a.str_imm(0, 22, plan.tcb_off + 8);
    a.movz(9, 2, 0); // Value::Null
    a.str_imm(9, 22, plan.packet_off);
    a.str_imm(31, 22, plan.packet_off + 8);
    emit_scheduler_active_drop_old_locals(a, layout, 25, 27);
}

/// Load the two stale wide-local owners into `old_tcb`/`old_packet` (zero represents a scalar)
/// and prove that replacing them can use bare shared-count decrements. This is deliberately
/// conservative: the rare BigInt/last-owner cases use the Rust materializer.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_guard_old_locals(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerActivePlan,
    old_tcb: u32,
    old_packet: u32,
    slow: usize,
) {
    let tcb_scalar = a.new_label();
    let tcb_ready = a.new_label();
    let packet_scalar = a.new_label();
    let owners_ready = a.new_label();
    let old_distinct = a.new_label();
    let counts_ready = a.new_label();
    let tcb_count_done = a.new_label();
    let strong = layout.rc_strong_off as i32;

    // Tags 0..=4 are non-owning. BigInt has a different representation; Str/Sym/Obj all use the
    // probed shared-reference strong-count layout and can be decremented inline while non-last.
    a.ldrb_imm(9, 22, plan.tcb_off);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_LO, tcb_scalar);
    a.b_cond(C_EQ, slow);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_HI, slow);
    a.ldr_imm(old_tcb, 22, plan.tcb_off + 8);
    a.b(tcb_ready);
    a.bind(tcb_scalar);
    a.movz(old_tcb, 0, 0);
    a.bind(tcb_ready);

    a.ldrb_imm(9, 22, plan.packet_off);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_LO, packet_scalar);
    a.b_cond(C_EQ, slow);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_HI, slow);
    a.ldr_imm(old_packet, 22, plan.packet_off + 8);
    a.b(owners_ready);
    a.bind(packet_scalar);
    a.movz(old_packet, 0, 0);
    a.bind(owners_ready);

    // A distinct old owner must have a survivor after its one decrement. If both locals alias,
    // their shared allocation needs a third owner because the inline path performs two bare
    // decrements. Source owners are cloned only after this guard, keeping failure side-effect free.
    a.cbz(old_tcb, true, old_distinct);
    a.cbz(old_packet, true, old_distinct);
    a.cmp_reg_x(old_tcb, old_packet);
    a.b_cond(C_NE, old_distinct);
    a.ldur(9, old_tcb, strong);
    a.cmp_imm_x(9, 2);
    a.b_cond(C_LS, slow);
    a.b(counts_ready);

    a.bind(old_distinct);
    a.cbz(old_tcb, true, tcb_count_done);
    a.ldur(9, old_tcb, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, slow);
    a.bind(tcb_count_done);
    a.cbz(old_packet, true, counts_ready);
    a.ldur(9, old_packet, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, slow);
    a.bind(counts_ready);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_active_drop_old_locals(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    old_tcb: u32,
    old_packet: u32,
) {
    let old_tcb_dropped = a.new_label();
    let old_packet_dropped = a.new_label();
    let strong = layout.rc_strong_off as i32;

    a.cbz(old_tcb, true, old_tcb_dropped);
    a.ldur(9, old_tcb, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, old_tcb, strong);
    a.bind(old_tcb_dropped);
    a.cbz(old_packet, true, old_packet_dropped);
    a.ldur(9, old_packet, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, old_packet, strong);
    a.bind(old_packet_dropped);
}

/// Scheduler-shell CFG region. x0 is the borrowed current TCB, x1 the scheduler owner, x2 the
/// scheduler's writable `current` entry, x5/x6 the live held/suspended globals, and x8 the pinned
/// expected TCB prototype. Every successful held step commits the scheduler property (including
/// exact Rc transfer) before the backedge, so any later guard can safely replay from `head`.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_scheduler_shell_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &SchedulerShellPlan,
    fast_loop: bool,
    plain_dispatch: Option<usize>,
    pc_labels: &[usize],
) -> (usize, Option<usize>) {
    let plain_h = a.new_label();
    let body = a.new_label();
    let fast_body = a.new_label();
    let graph_body = a.new_label();
    let graph_stale = a.new_label();
    let state_ready = a.new_label();
    let held = a.new_label();
    let link_null = a.new_label();
    let fast_resume = fast_loop.then(|| a.new_label());
    // A direct scheduler session executes no user code: every task arm is an exact-method,
    // guard-before-commit transaction, and every generic exit clears x28 before it can call JS.
    // Validate Active's state globals once at session entry, then use their observed integers in
    // the inner transaction instead of re-walking the global NameICs on every active TCB.
    let trust_active_names = fast_resume.is_some()
        && plan.active.is_some()
        && std::env::var_os("LUMEN_JIT_NO_SCHED_TRUST_NAMES").is_none();
    // A successful direct task transaction has already proved the current TCB's exact state
    // descriptor and stored an integral packed number. Until x28 is cleared no user code can
    // alter that descriptor/value contract, so resumptions can load the fixed state slot
    // directly instead of repeating the object/shape/attribute/type guard chain.
    let trust_state = fast_resume.is_some()
        && std::env::var_os("LUMEN_JIT_NO_SCHED_TRUST_STATE").is_none();
    // The bounded session contains no user code. Pin TaskControlBlock.run on the already-proved
    // common TCB prototype once, and lazily pin each task-role prototype in native-frame scratch.
    let role_epoch = fast_resume.is_some() && scheduler_role_epoch_enabled(plan);
    // Suspend/queue are the dominant nested method families. Their exact Scheduler and TCB
    // prototype entries can share the same bounded no-user-code lifetime without another cache.
    let method_epoch = fast_resume.is_some() && scheduler_method_epoch_enabled(plan);
    // A graph epoch eagerly validates all six exact TCB/task identities and selected own-entry
    // pointers before publishing x28. Every resume remaps the live Scheduler.current through the
    // table before one cached pointer is dereferenced.
    let graph_epoch =
        fast_resume.is_some() && scheduler_graph_epoch_enabled(plan, layout);
    // A soft graph extension proves all six role-local Scheduler edges. Runtime rejection leaves
    // the graph session intact and is tested through the header flag by the cached suspend tail.
    let graph_core = graph_epoch && scheduler_graph_core_enabled(plan);
    // The incoming Handler consumer has an independent A/B gate. It reuses CORE proof/header
    // state but retains the complete packet-list transaction and its existing native spill.
    let graph_core_incoming = graph_core && scheduler_graph_core_incoming_enabled(plan);
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let plain = layout.obj_ic_plain as u32;
    let shape = (layout.obj_props + layout.props_shape) as u32;
    let entries = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;

    debug_assert!(plan.head < plan.active_pc);
    // The inlined held-method receiver is compiler-only and dead after pc25. Keeping it
    // untouched is safe while it is any non-owning scalar (fresh inlined slots may be Empty or
    // Undefined). If an earlier guard replayed an iteration in the baseline it holds an object;
    // reject the region for the rest of this call rather than retaining a hidden extra root.
    a.ldrb_imm(9, 22, plan.temp_off);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_HS, plain_h);
    emit_region_name_i32(a, layout, plan.held_cache, 5, plain_h);
    emit_region_name_i32(a, layout, plan.suspended_cache, 6, plain_h);
    if trust_active_names {
        let active = plan.active.as_ref().expect("checked above");
        for (cache, expected) in [
            (active.suspended_runnable_cache, active.suspended_runnable),
            (active.running_cache, active.running),
            (active.runnable_cache, active.runnable),
        ] {
            emit_region_name_i32(a, layout, cache, 7, plain_h);
            a.mov_imm64(9, expected as u64);
            a.cmp_reg_x(7, 9);
            a.b_cond(C_NE, plain_h);
        }
    }

    // Scheduler `this.currentTcb`: the frame owns `this`; the packed property owns x0.
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(1, 14, 8);
    emit_region_own_entry(a, layout, 1, 3, 2, plan.current, true, plain_h);
    a.ldur(13, 2, ev);
    a.mov_imm64(14, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_EQ, pc_labels[plan.null_pc]);
    a.lsr_imm(9, 13, 48);
    a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_NE, plain_h);
    a.lsl_imm(0, 13, 16);
    a.lsr_imm(0, 0, 16);

    // Guard the inlined `isHeldOrSuspended` method once and pin its exact prototype pointer.
    // Each linked TCB below must have the same own shape and this same prototype object.
    a.add_imm(3, 0, rcv);
    a.ldrb_imm(9, 3, ex);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, plain_h);
    a.ldrb_imm(9, 3, plain);
    a.cbz(9, false, plain_h);
    a.ldr_w_imm(9, 3, shape);
    a.mov_imm64(14, plan.method.recv_shape as u64);
    a.cmp_reg_w(9, 14);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(8, 3, layout.obj_proto as u32);
    a.cbz(8, true, plain_h);
    emit_region_proto_method(
        a,
        layout,
        8,
        11,
        15,
        plan.method,
        plan.method_expected,
        plain_h,
    );
    if role_epoch {
        let active = plan.active.as_ref().expect("role epoch requires Active");
        emit_region_proto_method(
            a,
            layout,
            8,
            11,
            15,
            active.run_method,
            active.run_expected,
            plain_h,
        );
    }
    if method_epoch {
        let dispatch = plan
            .active
            .as_ref()
            .and_then(|active| active.null_dispatch.as_ref())
            .expect("method epoch requires null dispatch");
        let suspend = dispatch
            .device
            .suspend
            .as_ref()
            .expect("method epoch requires suspend plan");
        let queue = dispatch
            .device
            .queue
            .as_ref()
            .expect("method epoch requires queue plan");
        // Exact outer Scheduler prototype and its two hot methods. x7 is scratch until the
        // trusted globals are copied into x26/x27 below.
        a.add_imm(3, 1, rcv);
        a.ldr_imm(7, 3, layout.obj_proto as u32);
        a.cbz(7, true, plain_h);
        emit_region_proto_method(
            a,
            layout,
            7,
            11,
            15,
            suspend.suspend_method,
            suspend.suspend_expected,
            plain_h,
        );
        emit_region_proto_method(
            a,
            layout,
            7,
            11,
            15,
            queue.queue_method,
            queue.queue_expected,
            plain_h,
        );
        // x8 is the exact common TCB prototype already rooted by Scheduler.current/list/blocks.
        for (state, expected) in [
            (suspend.mark_method, suspend.mark_expected),
            (queue.check_method, queue.check_expected),
            (queue.mark_method, queue.mark_expected),
        ] {
            emit_region_proto_method(a, layout, 8, 11, 15, state, expected, plain_h);
        }
    }

    if graph_epoch {
        emit_scheduler_graph_epoch_fill(a, layout, plan, 1, 2, 8, plain_h);
        if graph_core {
            emit_scheduler_graph_core_fill(a, layout, plan, 1, 2, plain_h);
        }
    }

    if let Some(resume) = fast_resume {
        // These facts are invariant until a generic/user-code path clears x28. Every nested
        // scheduler transaction saves/restores x23..x28, so a successful direct task can return
        // here without repeating name, receiver, descriptor, and method guards.
        if role_epoch && !graph_epoch {
            a.stp_off(31, 31, SCHED_ROLE_DEVICE_PROTO_SP as i32);
            a.stp_off(31, 31, SCHED_ROLE_IDLE_PROTO_SP as i32);
        }
        a.mov(23, 1); // exact Scheduler owner (rooted by this_val)
        a.mov(24, 2); // writable Scheduler.current entry
        a.mov(25, 8); // exact TCB prototype (rooted by the live current/blocks graph)
        if graph_epoch {
            // The eager fill left x26 pointing at the exact live current record. Keep the state
            // globals in the graph header because x26 is now the hot current-record home.
            a.ldr_imm(27, 31, SCHED_GRAPH_SUSPENDED_SP);
        } else {
            a.mov(26, 5); // live STATE_HELD
            a.mov(27, 6); // live STATE_SUSPENDED
        }
        // Publication is last: every header/record/prototype word is complete before x28 != 0.
        a.movz(28, 1024, 0);
        a.b(if graph_epoch { graph_body } else { body });

        a.bind(resume);
        a.mov(1, 23);
        a.mov(2, 24);
        a.mov(8, 25);
        if graph_epoch {
            a.ldr_imm(5, 31, SCHED_GRAPH_HELD_SP);
            a.ldr_imm(6, 31, SCHED_GRAPH_SUSPENDED_SP);
        } else {
            a.mov(5, 26);
            a.mov(6, 27);
        }
        a.ldur(13, 2, ev);
        a.mov_imm64(14, crate::value::PACK_NULL);
        a.cmp_reg_x(13, 14);
        let fast_current = a.new_label();
        a.b_cond(C_NE, fast_current);
        a.movz(28, 0, 0);
        a.b(pc_labels[plan.null_pc]);
        a.bind(fast_current);
        a.lsr_imm(9, 13, 48);
        a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_NE, if graph_epoch { graph_stale } else { plain_h });
        a.lsl_imm(0, 13, 16);
        a.lsr_imm(0, 0, 16);
        if graph_epoch {
            // x26 restored by a nested transaction may describe the previous current. Check its
            // usual same-current case first, then scan all six exact identities on a miss.
            emit_scheduler_graph_find_record(a, 0, Some(26), 26, None, 0, graph_stale);
            a.b(graph_body);
        } else {
            a.b(if trust_state { fast_body } else { body });
        }
    }

    a.bind(body);
    emit_region_own_entry(a, layout, 0, 3, 4, plan.state, false, plain_h);
    a.ldr_imm(9, 3, layout.obj_proto as u32);
    a.cmp_reg_x(9, 8);
    a.b_cond(C_NE, plain_h);
    emit_region_packed_number(a, 4, ev, 0, plain_h);
    emit_region_exact_i32(a, 0, 7, plain_h);
    a.b(state_ready);

    a.bind(fast_body);
    // The full path above established this hidden-class slot and every direct continuation
    // either preserved it on the current TCB or guarded it on the newly selected target.
    a.add_imm(3, 0, rcv);
    a.ldr_imm(4, 3, entries);
    a.mov_imm64(13, plan.state.slot as u64);
    a.mov_imm64(10, layout.entry_size as u64);
    a.madd(4, 13, 10, 4);
    a.ldur_d(0, 4, ev);
    a.fcvtzs_w_d(7, 0);
    a.sxtw(7, 7);

    a.b(state_ready);

    a.bind(graph_body);
    // The fill proved this exact writable entry is a Number, and every direct transaction only
    // stores an integral Number. x26 was freshly identity-remapped on every external resume.
    a.ldr_imm(4, 26, SCHED_GRAPH_STATE_ENTRY_OFF);
    a.ldur_d(0, 4, ev);
    a.fcvtzs_w_d(7, 0);
    a.sxtw(7, 7);

    a.bind(state_ready);
    a.logic_w(0, 9, 7, 5);
    a.cbnz(9, false, held);
    a.cmp_reg_w(7, 6);
    a.b_cond(C_EQ, held);
    if let Some(active) = &plan.active {
        emit_scheduler_active_region(
            a,
            layout,
            active,
            4,
            7,
            8,
            plan.active_pc,
            fast_resume,
            trust_active_names,
            role_epoch,
            method_epoch,
            graph_epoch,
            graph_core,
            graph_core_incoming,
            plain_dispatch,
            pc_labels,
        );
    } else {
        a.b(pc_labels[plan.active_pc]);
    }

    a.bind(held);
    if graph_epoch {
        // TCB.link is immutable in every direct transaction. Pass two resolved it to an exact
        // native record pointer (or zero for Null), so no property/shape/tag guard remains here.
        a.ldr_imm(12, 26, SCHED_GRAPH_LINK_RECORD_OFF);
        a.cbz(12, true, link_null);
        a.ldr_imm(7, 12, SCHED_GRAPH_TCB_OFF);
    } else {
        emit_region_own_entry(a, layout, 0, 3, 4, plan.link, false, plain_h);
        a.ldur(13, 4, ev);
        a.mov_imm64(14, crate::value::PACK_NULL);
        a.cmp_reg_x(13, 14);
        a.b_cond(C_EQ, link_null);
        a.lsr_imm(9, 13, 48);
        a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
        a.cmp_reg_x(9, 14);
        a.b_cond(C_NE, plain_h);
        a.lsl_imm(7, 13, 16);
        a.lsr_imm(7, 7, 16);
    }
    // `current.link = current` is a clone followed by a drop of the same Rc. The scheduler
    // property already contains the identical packed word, so the exact net operation is a
    // no-op. Do not decrement using the stale pre-clone count (which would undercount/UAF).
    a.cmp_reg_x(7, 0);
    a.b_cond(
        C_EQ,
        if graph_epoch {
            graph_body
        } else if trust_state {
            fast_body
        } else {
            body
        },
    );
    // Clone the linked owner before replacing/decrementing the scheduler's old owner.
    a.ldur(9, 0, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, plain_h);
    a.ldur(10, 7, strong);
    a.add_imm(10, 10, 1);
    a.stur(10, 7, strong);
    if graph_epoch {
        a.mov_imm64(13, crate::value::PACK_OBJ);
        a.logic_x(1, 13, 13, 7);
    }
    a.stur(13, 2, ev);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);
    a.mov(0, 7);
    if graph_epoch {
        a.mov(26, 12);
        a.b(graph_body);
    } else {
        a.b(body);
    }

    a.bind(link_null);
    a.ldur(9, 0, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, plain_h);
    a.mov_imm64(13, crate::value::PACK_NULL);
    a.stur(13, 2, ev);
    a.sub_imm(9, 9, 1);
    a.stur(9, 0, strong);
    if fast_loop {
        a.movz(28, 0, 0);
    }
    a.b(pc_labels[plan.null_pc]);
    a.bind(graph_stale);
    a.movz(28, 0, 0);
    a.b(plain_h);
    (plain_h, fast_resume)
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_region_name_i32(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    cache: usize,
    out: u32,
    fail: usize,
) {
    emit_name_ic_value_ptr(a, layout, cache, fail, true);
    let decoded = a.new_label();
    let wide = a.new_label();
    a.cbz(7, false, wide);
    emit_region_packed_number(a, 14, 0, 0, fail);
    a.b(decoded);
    a.bind(wide);
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, fail);
    a.ldur_d(0, 14, 8);
    a.bind(decoded);
    emit_region_exact_i32(a, 0, out, fail);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_numeric_diamond_flush(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &NumericDiamondPlan,
) {
    // Both locations were entry-proven Numbers and no helper ran in-region, so payload-only
    // writeback preserves their tags and requires no ownership work.
    a.scvtf_d_x(0, 0);
    a.str_d_imm(0, 22, plan.index_off + 8);
    a.scvtf_d_x(1, 8);
    a.fmov_x_d(9, 1);
    a.stur(9, 2, layout.entry_value as i32);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_linked_scan_materialize(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &LinkedScanPlan,
    peek_object: bool,
) {
    let strong = layout.rc_strong_off as i32;
    // x0 is a borrowed current-node Rc pointer.  Create the one or two frame owners before
    // releasing either old slot, so aliasing and descendant pointers remain live throughout.
    a.ldur(9, 0, strong);
    a.add_imm(9, 9, if peek_object { 2 } else { 1 });
    a.stur(9, 0, strong);
    a.movz(9, 8, 0); // Value::Obj
    a.str_imm(9, 22, plan.next_off);
    a.str_imm(0, 22, plan.next_off + 8);
    if peek_object {
        a.str_imm(9, 22, plan.peek_off);
        a.str_imm(0, 22, plan.peek_off + 8);
    } else {
        a.movz(9, 2, 0); // Value::Null
        a.str_imm(9, 22, plan.peek_off);
        a.str_imm(31, 22, plan.peek_off + 8);
    }

    // Preamble guards proved these decrements cannot hit the last reference, including when the
    // old next/peek aliases.  x3=old next payload, w4/x5=old peek tag/payload.
    a.ldur(9, 3, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 3, strong);
    let scalar_peek = a.new_label();
    a.cmp_imm_w(4, 6);
    a.b_cond(C_LO, scalar_peek);
    a.ldur(9, 5, strong);
    a.sub_imm(9, 9, 1);
    a.stur(9, 5, strong);
    a.bind(scalar_peek);
}

/// Linked-list SSA region.  The initial `next` frame owner roots the complete property chain, so
/// x0 can walk borrowed packed object pointers without a clone/drop pair at every node.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_linked_scan_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &LinkedScanPlan,
    pc_labels: &[usize],
) -> usize {
    let plain_h = a.new_label();
    let body = a.new_label();
    let fail = a.new_label();
    let found_null = a.new_label();
    let strong = layout.rc_strong_off as i32;

    // Pin the old slot owners for one-time replacement.  Reject BigInt and any reference whose
    // decrement might invoke a destructor; the baseline path remains untouched on rejection.
    a.ldrb_imm(9, 22, plan.next_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(3, 22, plan.next_off + 8);
    a.ldur(9, 3, strong);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, plain_h);
    a.ldrb_imm(4, 22, plan.peek_off);
    a.ldr_imm(5, 22, plan.peek_off + 8);
    a.cmp_imm_w(4, 5);
    a.b_cond(C_EQ, plain_h); // BigInt has a different ownership representation
    let peek_safe = a.new_label();
    a.cmp_imm_w(4, 6);
    a.b_cond(C_LO, peek_safe);
    a.ldur(9, 5, strong);
    a.cmp_reg_x(5, 3);
    let distinct = a.new_label();
    a.b_cond(C_NE, distinct);
    a.cmp_imm_x(9, 2); // two old frame owners of the same allocation
    a.b_cond(C_LS, plain_h);
    a.b(peek_safe);
    a.bind(distinct);
    a.cmp_imm_x(9, 1);
    a.b_cond(C_LS, plain_h);
    a.bind(peek_safe);

    a.mov(0, 3); // borrowed current node
    a.movz(8, 0, 0); // no successful assignment yet
    a.bind(body);
    emit_region_own_entry(a, layout, 0, 1, 2, plan.link, false, fail);
    a.ldur(13, 2, layout.entry_value as i32);
    a.mov_imm64(14, crate::value::PACK_NULL);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_EQ, found_null);
    a.lsr_imm(9, 13, 48);
    a.movz(14, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_NE, fail);
    a.lsl_imm(15, 13, 16);
    a.lsr_imm(15, 15, 16);
    if plan.loose_null_compare {
        // Loose equality has the one object/null exception: an HTMLDDA object compares equal to
        // null.  `ic_plain` is false for that object (and for other exotic receivers), so replay
        // the comparison in the baseline before committing `next = peek`.  Keep x0 unchanged
        // until after this guard so the materialized loop-head state remains exact.
        a.add_imm(14, 15, layout.obj_from_rc as u32);
        a.ldrb_imm(9, 14, layout.obj_ic_plain as u32);
        a.cbz(9, false, fail);
    }
    a.mov(0, 15);
    a.movz(8, 1, 0);
    a.b(body);

    a.bind(found_null);
    emit_linked_scan_materialize(a, layout, plan, false);
    a.b(pc_labels[plan.exit_pc]);

    a.bind(fail);
    a.cbz(8, false, plain_h); // first iteration: the original frame is still canonical
    emit_linked_scan_materialize(a, layout, plan, true);
    a.b(plain_h); // later failure: resume baseline at the loop head with materialized state
    plain_h
}

/// Emit the first non-linear region tier.  x0=index, x1=limit, x2=counter entry, x3=array body,
/// x4=mirror data, x5=mirror length, x6=index→entry map, x7=entry data, x8=counter.  The region
/// is helper-free; frame/property owners remain GC roots for all borrowed pointers.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_numeric_diamond_region(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &NumericDiamondPlan,
    pc_labels: &[usize],
) -> usize {
    let plain_h = a.new_label();
    let body = a.new_label();
    let exit = a.new_label();
    let store_bail = a.new_label();
    let ev = layout.entry_value as i32;

    // Invariant free-name limit.  Validation comes first because the shared name probe clobbers
    // x9-x17; fixed homes are populated afterwards.
    emit_region_name_i32(a, layout, plan.limit_cache, 1, plain_h);

    // Resolve `owner.array` before populating caller-saved fixed homes. The optional C-ABI
    // protector check may clobber x0-x18; afterwards x4 holds its packed Vec header (or null),
    // x5 keeps the borrowed array Rc pointer, and the remainder of the preamble is helper-free.
    a.ldrb_imm(9, 22, plan.owner_off);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(10, 22, plan.owner_off + 8);
    emit_region_own_entry(a, layout, 10, 11, 12, plan.array_prop, false, plain_h);
    a.ldur(13, 12, ev);
    a.lsr_imm(9, 13, 48);
    a.movz(16, (crate::value::PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_NE, plain_h);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    a.stp_pre(23, 24, -16);
    a.mov(23, 1);
    a.mov(24, 13);
    a.mov(0, 19);
    a.add_imm(1, 13, layout.gc_data_off as u32);
    a.mov(2, 23);
    a.mov_imm64(
        16,
        jit_prepare_numeric_packed_array as *const () as usize as u64,
    );
    a.blr(16);
    a.mov(4, 0);
    a.mov(1, 23);
    a.mov(5, 24);
    a.ldp_post(23, 24, 16);

    // Numeric induction local, exact i32 and non-negative (dense element key invariant).
    a.ldrb_imm(9, 22, plan.index_off);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, plain_h);
    a.ldr_d_imm(0, 22, plan.index_off + 8);
    emit_region_exact_i32(a, 0, 0, plain_h);
    a.cmp_imm_x(0, 0);
    a.b_cond(C_MI, plain_h);
    a.cmp_imm_x(1, 0);
    a.b_cond(C_MI, plain_h);

    // Stable own numeric `this` field.  The frame's this Value roots the receiver; x2 points at
    // the packed Property word for deferred writeback.
    a.ldr_imm(14, 19, 48);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, plain_h);
    a.ldr_imm(10, 14, 8);
    emit_region_own_entry(a, layout, 10, 11, 2, plan.counter, true, plain_h);
    emit_region_packed_number(a, 2, ev, 0, plain_h);
    emit_region_exact_i32(a, 0, 8, plain_h);

    // Pin either the packed slots or the coherent classic mirror/entry tables for the loop.
    a.mov(13, 5);
    a.add_imm(3, 13, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 3, layout.obj_exotic as u32);
    let array_kind = a.new_label();
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_EQ, array_kind);
    a.cmp_imm_w(9, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, plain_h);
    a.bind(array_kind);
    a.ldrb_imm(9, 3, layout.obj_ic_plain as u32);
    a.cbz(9, false, plain_h);
    let packed_array = a.new_label();
    let array_ready = a.new_label();
    a.cbnz(4, true, packed_array);
    let mirror_flags = (layout.obj_props + layout.props_mirror_flags) as u32;
    a.ldrb_imm(9, 3, mirror_flags);
    let need = (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32;
    let mask = asm::logical_imm_w(need).expect("mirror region mask");
    a.logic_imm_w(0, 9, 9, mask);
    a.cmp_imm_w(9, need);
    a.b_cond(C_NE, plain_h);
    let dense_off = (layout.obj_props + layout.props_elems) as u32;
    a.ldr_imm(12, 3, dense_off);
    a.cbz(12, true, plain_h);
    a.ldr_imm(
        4,
        12,
        (layout.dense_mirror + layout.vec_ptr_off) as u32,
    );
    a.ldr_imm(
        5,
        12,
        (layout.dense_mirror + layout.vec_len_off) as u32,
    );
    a.ldr_imm(
        6,
        12,
        (layout.dense_elems + layout.vec_ptr_off) as u32,
    );
    a.ldr_imm(
        7,
        3,
        (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32,
    );
    a.b(array_ready);
    a.bind(packed_array);
    a.ldr_imm(5, 4, layout.vec_len_off as u32);
    a.ldr_imm(4, 4, layout.vec_ptr_off as u32);
    a.movz(6, 0, 0); // mode marker: no classic index-to-entry vector
    a.movz(7, 0, 0);
    a.bind(array_ready);
    a.cmp_reg_x(1, 5);
    a.b_cond(C_HI, plain_h); // every possible loop key is inside the pinned mirror

    // Rotated loop: the zero-trip condition observes no property/element write.
    a.cmp_reg_x(0, 1);
    a.b_cond(C_GE, exit);
    a.bind(body);
    a.add_imm(8, 8, 1); // this.counter++
    a.mov_imm64(9, plan.threshold as u64);
    a.cmp_reg_x(8, 9);
    let no_reset = a.new_label();
    a.b_cond(13, no_reset); // signed <=
    a.mov_imm64(8, plan.reset as u64);
    a.bind(no_reset);

    // array[index] = counter.  NO_HOLES and limit<=mirror length make bounds invariant; retain a
    // defensive NO_SLOT exit in case layout invariants ever change under us.
    let packed_store = a.new_label();
    let stored = a.new_label();
    a.cbz(6, true, packed_store);
    a.add_shifted(12, 6, 0, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, store_bail);
    a.mov_imm64(14, layout.entry_size as u64);
    a.madd(15, 13, 14, 7);
    a.scvtf_d_x(0, 8);
    let num_ev = if layout.entry_accessor == layout.entry_value + 8 {
        ev
    } else {
        ev + 8
    };
    a.stur_d(0, 15, num_ev);
    a.str_d_lsl3(0, 4, 0);
    a.b(stored);
    a.bind(packed_store);
    a.add_shifted(15, 4, 0, 4); // packed Property stride is 16 bytes
    a.scvtf_d_x(0, 8);
    a.stur_d(0, 15, layout.property_value as i32);
    a.bind(stored);

    a.add_imm(0, 0, 1);
    a.cmp_reg_x(0, 1);
    a.b_cond(11, body); // signed <
    a.bind(exit);
    emit_numeric_diamond_flush(a, layout, plan);
    a.b(pc_labels[plan.exit_pc]);

    // The update/reset has logically committed, while the element operation has not.  Restore
    // canonical frame state and resume at GetPropLocal, exactly after those committed effects.
    a.bind(store_bail);
    emit_numeric_diamond_flush(a, layout, plan);
    a.b(pc_labels[plan.head + 12]);
    plain_h
}

/// Emit the loop chain for `plan`. Returns the label for the plain fallback of the head op —
/// the caller binds it immediately after and continues emitting the plain region.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_loop_chain(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    plan: &LoopPlan,
    pc_labels: &[usize],
) -> usize {
    let strong = layout.rc_strong_off as i32;
    let rcv = layout.obj_from_rc as u32;
    let ex = layout.obj_exotic as u32;
    let el = (layout.obj_props + layout.props_elems) as u32;
    let evp = (layout.dense_elems + layout.vec_ptr_off) as u32;
    let evl = (layout.dense_elems + layout.vec_len_off) as u32;
    let mvp = (layout.dense_mirror + layout.vec_ptr_off) as u32;
    let mvl = (layout.dense_mirror + layout.vec_len_off) as u32;
    let en = (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32;
    let ev = layout.entry_value as i32;
    let num_ev = if layout.entry_accessor == layout.entry_value + 8 {
        ev
    } else {
        ev + 8
    };
    let ea = layout.entry_accessor as u32;
    let ew = layout.entry_writable as u32;
    let es = layout.entry_size as u64;
    let none_tag = layout.exotic_none_tag as u32;
    let arr_tag = layout.exotic_array_tag as u32;
    let plain = layout.obj_ic_plain as u32;
    let mf = (layout.obj_props + layout.props_mirror_flags) as u32;
    let mirror = el;

    let plain_h = a.new_label();
    let body_l = a.new_label();
    let exit_a = a.new_label();
    let exit_b = a.new_label();
    // x23-x28 bracket (see LoopPlan::uses_ext): saved before ANY preamble step (receiver bases
    // may live in ext registers), reloaded on every path out — preamble failures route through
    // `pre_fail`, exits and bails emit the reload inline.
    let pre_fail = if plan.uses_ext {
        a.new_label()
    } else {
        plain_h
    };
    if plan.uses_ext {
        a.stp_pre(23, 24, -48);
        a.stp_off(25, 26, 16);
        a.stp_off(27, 28, 32);
    }
    let restore_ext = |a: &mut asm::Asm| {
        a.ldp_off(25, 26, 16);
        a.ldp_off(27, 28, 32);
        a.ldp_post(23, 24, 48);
    };

    let slot = |off: u32| plan.slots.iter().find(|s| s.off == off);
    let rcv_plan = |off: u32| plan.receivers.iter().find(|r| r.off == off);
    // Virgins stored within the condition prefix (they flush even on the entry exit).
    let cond_virgins: Vec<u32> = plan.chain[..plan.cond_len]
        .iter()
        .filter_map(|(c, _)| match *c {
            ChainOp::Store(off) if slot(off).is_some_and(|s| s.virgin) => Some(off),
            _ => None,
        })
        .collect();
    let all_virgins: Vec<u32> = plan
        .slots
        .iter()
        .filter(|s| s.virgin)
        .map(|s| s.off)
        .collect();

    // ---- preamble --------------------------------------------------------------------------
    // Validate names before populating integer local homes: the shared IC probe uses x7 as its
    // packed/wide result marker in addition to x9-x17. Large regions can legitimately assign
    // a resident local to x7, so reversing this order would silently overwrite that local.
    for np in &plan.names {
        emit_name_ic_value_ptr(a, layout, np.ptr, pre_fail, true);
        let loaded = a.new_label();
        if layout.entry_accessor == layout.entry_value + 8 {
            let wide = a.new_label();
            a.cbz(7, false, wide);
            a.ldur(9, 14, 0);
            a.lsr_imm(10, 9, 48);
            let number = a.new_label();
            a.movz(11, (crate::value::PACK_OBJ >> 48) as u32, 0);
            a.cmp_reg_x(10, 11);
            a.b_cond(C_EQ, pre_fail);
            a.movz(11, (crate::value::PACK_UNDEFINED >> 48) as u32, 0);
            a.cmp_reg_x(10, 11);
            a.b_cond(C_LO, number);
            a.movz(11, (crate::value::PACK_SYM >> 48) as u32, 0);
            a.cmp_reg_x(10, 11);
            a.b_cond(C_LS, pre_fail);
            a.bind(number);
            a.fmov_d_x(np.dreg, 9);
            a.b(loaded);
            a.bind(wide);
        }
        a.ldurb(9, 14, 0);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, pre_fail); // only a Num can live in a register
        a.ldur_d(np.dreg, 14, 8);
        a.bind(loaded);
        if np.int_checked {
            a.fcvtzs_w_d(9, np.dreg);
            a.scvtf_d_w(1, 9);
            a.fmov_x_d(10, 1);
            a.fmov_x_d(11, np.dreg);
            a.cmp_reg_x(10, 11);
            a.b_cond(C_NE, pre_fail);
        }
    }
    for s in &plan.slots {
        if s.virgin {
            // The old value must be drop-free so flushes can plain-overwrite.
            a.ldrb_imm(9, 22, s.off);
            a.cmp_imm_w(9, 5);
            a.b_cond(C_HS, pre_fail);
        }
        if !s.preload {
            continue;
        }
        a.ldrb_imm(9, 22, s.off);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, pre_fail);
        match s.res {
            SlotRes::F(d) => {
                a.ldr_d_imm(d, 22, s.off + 8);
                if s.int_checked {
                    // One-time exact-i32 proof (bit-compare: -0.0 must not pass); the value
                    // stays in its d home and integer consumers convert with a bare fcvtzs.
                    a.fcvtzs_w_d(9, d);
                    a.scvtf_d_w(1, 9);
                    a.fmov_x_d(10, 1);
                    a.fmov_x_d(11, d);
                    a.cmp_reg_x(10, 11);
                    a.b_cond(C_NE, pre_fail);
                }
            }
            SlotRes::I(x) => {
                // Exact i32 (w-form conversion + compare-back): counters keep the invariant
                // with a flag-setting ±1, and the planner's range analysis starts from 2^31.
                a.ldr_d_imm(0, 22, s.off + 8);
                a.fcvtzs_w_d(x, 0);
                a.scvtf_d_w(1, x);
                a.fmov_x_d(9, 1);
                a.fmov_x_d(10, 0);
                a.cmp_reg_x(9, 10);
                a.b_cond(C_NE, pre_fail);
                a.sxtw(x, x);
            }
            SlotRes::None => {}
        }
    }
    // Receiver bases come last because the name probe also clobbers x16/x17.
    for rp in &plan.receivers {
        let (off, r) = (rp.off, rp.reg);
        a.ldrb_imm(10, 22, off);
        a.cmp_imm_w(10, 8);
        a.b_cond(C_NE, pre_fail);
        a.ldr_imm(10, 22, off + 8);
        a.add_imm(r, 10, rcv);
        a.ldrb_imm(12, r, ex);
        let ex_ok = a.new_label();
        a.cmp_imm_w(12, none_tag);
        a.b_cond(C_EQ, ex_ok);
        a.cmp_imm_w(12, arr_tag);
        a.b_cond(C_NE, pre_fail);
        a.bind(ex_ok);
        a.ldrb_imm(12, r, plain);
        a.cbz(12, false, pre_fail);
        if rp.mirror {
            // The element buffer must be coherent, hole-free, and (for int-read receivers)
            // all-i32: element reads become bounds + one indexed load with no tag check.
            let mut need = (crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES) as u32;
            if rp.int_reads {
                need |= crate::value::MIRROR_ALL_I32 as u32;
            }
            a.ldrb_imm(12, r, mf);
            let field = asm::logical_imm_w(need).expect("mirror mask encodable");
            a.logic_imm_w(0, 12, 12, field);
            a.cmp_imm_w(12, need);
            a.b_cond(C_NE, pre_fail);
        }
        // Pinned vector fields (stable for the whole region — helper-free vocabulary, and slim
        // stores never grow or reallocate).
        if rp.mlreg.is_some() || rp.mpreg.is_some() {
            a.ldr_imm(12, r, mirror);
            a.cbz(12, true, pre_fail);
            if let Some(x) = rp.mlreg {
                a.ldr_imm(x, 12, mvl);
            }
            if let Some(x) = rp.mpreg {
                a.ldr_imm(x, 12, mvp);
            }
        }
        if let Some(x) = rp.elpreg {
            a.ldr_imm(12, r, el);
            a.cbz(12, true, pre_fail);
            a.ldr_imm(x, 12, evp);
        }
        if let Some(x) = rp.enreg {
            a.ldr_imm(x, r, en);
        }
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
        .filter(|x| {
            !plan.slots.iter().any(|s| s.res == SlotRes::I(*x))
                && !pinned(*x)
                && !plan.receivers.iter().any(|r| {
                    r.reg == *x || [r.mlreg, r.mpreg, r.elpreg, r.enreg].contains(&Some(*x))
                })
        })
        .collect();
    let mut free_d: Vec<u32> = (16..24).rev().collect();
    // Loads push ALIASES of resident registers (zero-copy: consumers never clobber their
    // operands — Arith/Neg/Bit write fresh destinations). The pools only take back their own:
    // a freed alias of an I/F home or a name home silently stays out.
    let pool_i: Vec<u32> = free_i.clone();
    let is_pool_i = |x: u32| pool_i.contains(&x);
    let is_pool_d = |d: u32| (16..24).contains(&d);

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
                // Materialize any live vstack alias of a resident register about to be
                // overwritten (an Update/Store to its slot): the pushed value must keep the
                // OLD contents. Runs after the pre-op snapshot (bails read the still-unmutated
                // resident) and before the mutation.
                macro_rules! flush_aliases {
                    ($home:expr, $is_f:expr) => {{
                        for k in 0..vstack.len() {
                            match vstack[k] {
                                LV::D(d, iv) if $is_f && d == $home => {
                                    let dt = free_d.pop().expect("loop d pool");
                                    a.fmov_d_d(dt, d);
                                    vstack[k] = LV::D(dt, iv);
                                }
                                LV::I(x, ng) if !$is_f && x == $home => {
                                    let xt = free_i.pop().expect("loop i pool");
                                    a.mov(xt, x);
                                    vstack[k] = LV::I(xt, ng);
                                }
                                _ => {}
                            }
                        }
                    }};
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
                            LV::I(x, _neg) => {
                                // No explicit negative check: LV::I is a sign-extended exact
                                // i32, and every consumer's FIRST use of x9 is an unsigned
                                // bounds compare against a vector length — a negative reads
                                // as ≥ 2^63 and takes the same bail the explicit check did.
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
                        a.ldr_imm(12, $r, el);
                        a.cbz(12, true, guard!());
                        a.ldr_imm(14, 12, evl);
                        a.cmp_reg_x(9, 14);
                        a.b_cond(C_HS, guard!());
                        a.ldr_imm(12, 12, evp);
                        a.add_shifted(12, 12, 9, 2);
                        a.ldr_w_imm(13, 12, 0);
                        a.cmn_imm_w(13, 1);
                        a.b_cond(C_EQ, guard!());
                        a.ldr_imm(15, $r, en);
                        a.movz(9, es as u32, 0);
                        a.madd(15, 13, 9, 15);
                        guard_prop_data(a, 9, 15, ea, guard!());
                    }};
                }

                match *cop {
                    ChainOp::ConstNum(bits) => vstack.push(LV::K(bits)),
                    ChainOp::Load(off) => {
                        let s = slot(off).expect("planned slot");
                        match s.res {
                            SlotRes::F(dres) => {
                                // Zero-copy alias of the home (see the pool filter).
                                let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                                vstack.push(LV::D(dres, iv));
                            }
                            SlotRes::I(xres) => {
                                vstack.push(LV::I(xres, true));
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
                        match s.res {
                            SlotRes::F(d) => flush_aliases!(d, true),
                            SlotRes::I(x) => flush_aliases!(x, false),
                            SlotRes::None => {}
                        }
                        let dec = matches!(
                            kind,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
                        );
                        match s.res {
                            SlotRes::I(xres) => {
                                // The entry guard proved exact i32; a flag-setting w-form ±1
                                // keeps it (V = left i32 = bail), far from f64's 2^53 edge.
                                // The guard fires before any mutation, so the sign-extend can
                                // land straight in the resident.
                                if dec {
                                    a.subs_imm_w(9, xres, 1);
                                } else {
                                    a.adds_imm_w(9, xres, 1);
                                }
                                a.b_cond(C_VS, guard!());
                                match kind {
                                    UpdKind::PostInc | UpdKind::PostDec => {
                                        let xt = free_i.pop().expect("loop i pool");
                                        a.mov(xt, xres);
                                        a.sxtw(xres, 9);
                                        vstack.push(LV::I(xt, true));
                                    }
                                    UpdKind::PreInc | UpdKind::PreDec => {
                                        a.sxtw(xres, 9);
                                        vstack.push(LV::I(xres, true));
                                    }
                                    _ => a.sxtw(xres, 9),
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
                        if let Some(&(_, ridx)) = plan.elem_reuse.iter().find(|&&(d, _)| d == idx) {
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
                            let rp = rcv_plan(xoff).expect("planned receiver");
                            let pin = plan
                                .elem_retain
                                .iter()
                                .find(|&&(i, _)| i == idx)
                                .map(|p| p.1);
                            if rp.mirror {
                                // Mirror: bounds + one indexed load. Preamble proved coherent
                                // + hole-free (+ all-i32 for int reads): no tag check, and int
                                // reads need no exactness guard. Pinned length/data registers
                                // shave the two dependent loads when the planner had room.
                                match rp.mlreg {
                                    Some(x) => a.cmp_reg_x(9, x),
                                    None => {
                                        a.ldr_imm(12, rp.reg, mirror);
                                        a.cbz(12, true, guard!());
                                        a.ldr_imm(14, 12, mvl);
                                        a.cmp_reg_x(9, 14);
                                    }
                                }
                                a.b_cond(C_HS, guard!());
                                let mpr = match rp.mpreg {
                                    Some(x) => x,
                                    None => {
                                        a.ldr_imm(12, rp.reg, mirror);
                                        a.cbz(12, true, guard!());
                                        a.ldr_imm(14, 12, mvp);
                                        14
                                    }
                                };
                                if matches!(plan.kinds[idx], PushKind::I { .. }) {
                                    a.ldr_d_lsl3(0, mpr, 9);
                                    let xt = free_i.pop().expect("loop i pool");
                                    a.fcvtzs_w_d(xt, 0);
                                    a.sxtw(xt, xt);
                                    if let Some(p) = pin {
                                        a.mov(p, xt);
                                    }
                                    vstack.push(LV::I(xt, true));
                                } else {
                                    let dt = free_d.pop().expect("loop d pool");
                                    a.ldr_d_lsl3(dt, mpr, 9);
                                    if let Some(p) = pin {
                                        a.fmov_d_d(p, dt);
                                    }
                                    vstack.push(LV::D(dt, false));
                                }
                            } else {
                                let r = rp.reg;
                                if layout.entry_accessor == layout.entry_value + 8 {
                                    a.b(guard!());
                                }
                                elem_entry!(r);
                                a.ldrb_imm(9, 15, ev as u32);
                                a.cmp_imm_w(9, 4);
                                a.b_cond(C_NE, guard!());
                                if matches!(plan.kinds[idx], PushKind::I { .. }) {
                                    // w-form: the exactness compare-back also proves i32 (the
                                    // planner's range analysis relies on that bound).
                                    a.ldur_d(0, 15, ev + 8);
                                    let xt = free_i.pop().expect("loop i pool");
                                    a.fcvtzs_w_d(xt, 0);
                                    a.scvtf_d_w(1, xt);
                                    // Bit-compare, not fcmp: IEEE equality would accept -0.0
                                    // and erase its sign through the int-typed value.
                                    a.fmov_x_d(9, 1);
                                    a.fmov_x_d(10, 0);
                                    a.cmp_reg_x(9, 10);
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
                        let rp = rcv_plan(xoff).expect("planned receiver");
                        let r = rp.reg;
                        let i32_proven = plan.setelem_i32.get(&idx).copied().unwrap_or(false);
                        if rp.mirror {
                            // Mirror invariant (preamble-proven): every mirrored element is a
                            // plain writable data Num — the accessor/writable/old-value checks
                            // and the tag write all collapse. A hole (elems NO_SLOT) bails:
                            // that store would CREATE a property. Pinned vector registers
                            // shave the four dependent loads when the planner had room.
                            match rp.mlreg {
                                Some(x) => a.cmp_reg_x(9, x),
                                None => {
                                    a.ldr_imm(12, r, mirror);
                                    a.cbz(12, true, guard!());
                                    a.ldr_imm(14, 12, mvl);
                                    a.cmp_reg_x(9, 14);
                                }
                            }
                            a.b_cond(C_HS, guard!());
                            let elpr = match rp.elpreg {
                                Some(x) => x,
                                None => {
                                    a.ldr_imm(12, r, el);
                                    a.cbz(12, true, guard!());
                                    a.ldr_imm(14, 12, evp);
                                    14
                                }
                            };
                            a.add_shifted(12, elpr, 9, 2);
                            a.ldr_w_imm(13, 12, 0);
                            a.cmn_imm_w(13, 1);
                            a.b_cond(C_EQ, guard!());
                            let enr = match rp.enreg {
                                Some(x) => x,
                                None => {
                                    a.ldr_imm(15, r, en);
                                    15
                                }
                            };
                            a.movz(12, es as u32, 0);
                            a.madd(15, 13, 12, enr);
                            a.stur_d(2, 15, num_ev);
                            match rp.mpreg {
                                Some(x) => a.str_d_lsl3(2, x, 9),
                                None => {
                                    a.ldr_imm(12, r, mirror);
                                    a.ldr_imm(12, 12, mvp);
                                    a.str_d_lsl3(2, 12, 9);
                                }
                            }
                            if !i32_proven {
                                // MIRROR_ALL_I32 upkeep, flag-first (no sentinel screen —
                                // hole accounting is structural, see Props::mirror_sync).
                                let i32_done = a.new_label();
                                a.ldrb_imm(13, r, mf);
                                let i32_bit =
                                    asm::logical_imm_w(crate::value::MIRROR_ALL_I32 as u32)
                                        .unwrap();
                                a.logic_imm_w(0, 12, 13, i32_bit);
                                a.cbz(12, false, i32_done);
                                a.fcvtzs_w_d(12, 2);
                                a.scvtf_d_w(1, 12);
                                a.fmov_x_d(12, 1);
                                a.fmov_x_d(14, 2);
                                a.cmp_reg_x(12, 14);
                                a.b_cond(C_EQ, i32_done);
                                let clear =
                                    asm::logical_imm_w(!(crate::value::MIRROR_ALL_I32 as u32))
                                        .unwrap();
                                a.logic_imm_w(0, 13, 13, clear);
                                a.strb_imm(13, r, mf);
                                a.bind(i32_done);
                            }
                        } else {
                            if layout.entry_accessor == layout.entry_value + 8 {
                                a.b(guard!());
                            }
                            elem_entry!(r);
                            guard_prop_writable(a, 9, 15, ew, guard!());
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
                            let no_dec = a.new_label();
                            a.cmp_imm_w(14, 6);
                            a.b_cond(C_LO, no_dec);
                            a.ldur(13, 12, strong);
                            a.sub_imm(13, 13, 1);
                            a.stur(13, 12, strong);
                            a.bind(no_dec);
                            // Element mirror: the value was staged in d2; key registers are
                            // still intact (operand frees are deferred to op end).
                            let mkey = match key {
                                LV::I(x, _) => MirrorKey::U32InReg(x),
                                LV::D(d, _) => MirrorKey::F64InDreg(d),
                                // A K key reaching the commit passed the exact-u32 runtime
                                // check, so the compile-time conversion is exact.
                                LV::K(bits) => MirrorKey::Const(f64::from_bits(bits) as u32),
                            };
                            emit_mirror_store(a, layout, r, mkey, MirrorVal::Num(2, i32_proven));
                        }
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
                            // Fresh destination: operands may be zero-copy aliases of resident
                            // registers (f_arith is 3-operand, so this costs nothing).
                            let db = to_d!(b);
                            let da = to_d!(a_);
                            let dt = free_d.pop().expect("loop d pool");
                            a.f_arith(f, dt, da, db);
                            dead.push(LV::D(da, false));
                            dead.push(LV::D(db, false));
                            let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                            vstack.push(LV::D(dt, iv));
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
                        let dt = free_d.pop().expect("loop d pool");
                        a.fneg(dt, d);
                        dead.push(LV::D(d, false));
                        let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                        vstack.push(LV::D(dt, iv));
                    }
                    ChainOp::Store(off) => {
                        let v = vstack.pop().expect("loop vstack");
                        let s = slot(off).expect("planned slot");
                        match s.res {
                            SlotRes::F(d) => flush_aliases!(d, true),
                            SlotRes::I(x) => flush_aliases!(x, false),
                            SlotRes::None => {}
                        }
                        match s.res {
                            SlotRes::F(dres) => match v {
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
                            },
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
                            // Aliases duplicate for free (nothing clobbers them; the pool
                            // filter blocks their double-free). Owned temps still copy — the
                            // two entries free independently.
                            LV::I(x, neg) => {
                                if is_pool_i(x) {
                                    let xt = free_i.pop().expect("loop i pool");
                                    a.mov(xt, x);
                                    vstack.push(LV::I(xt, neg));
                                } else {
                                    vstack.push(LV::I(x, neg));
                                }
                            }
                            LV::D(d, iv) => {
                                if is_pool_d(d) {
                                    let dt = free_d.pop().expect("loop d pool");
                                    a.fmov_d_d(dt, d);
                                    vstack.push(LV::D(dt, iv));
                                } else {
                                    vstack.push(LV::D(d, iv));
                                }
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
                            // One side exact-int in a register, the other a PROVEN-integral
                            // f64 (an int-checked name/preload): a bare x-form fcvtzs is exact,
                            // so the compare stays integer — the loop-head `i < width` pattern,
                            // otherwise a per-iteration scvtf + fcmp on the branch path.
                            (LV::I(xa, _), LV::D(db, true)) => {
                                a.fcvtzs_x_d(9, db);
                                a.cmp_reg_x(xa, 9);
                                a.b_cond(int_neg, $exit);
                                dead.push(LV::I(xa, false));
                                dead.push(LV::D(db, false));
                            }
                            (LV::D(da, true), LV::I(xb, _)) => {
                                a.fcvtzs_x_d(9, da);
                                a.cmp_reg_x(9, xb);
                                a.b_cond(int_neg, $exit);
                                dead.push(LV::D(da, false));
                                dead.push(LV::I(xb, false));
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
                    ChainOp::LoadName(ptr) => {
                        // Preamble-pinned, never written in-region: a zero-copy alias.
                        let np = plan
                            .names
                            .iter()
                            .find(|n| n.ptr == ptr)
                            .expect("planned name");
                        let iv = matches!(plan.kinds[idx], PushKind::D { iv: true });
                        vstack.push(LV::D(np.dreg, iv));
                    }
                    ChainOp::LoadProp(..) | ChainOp::StoreProp(..) => {
                        unreachable!("loop discovery never admits property operations")
                    }
                }
                if used {
                    bails.push((idx, bail, snap, seen_snap));
                }
                for v in dead {
                    match v {
                        LV::I(x, _) if is_pool_i(x) => free_i.push(x),
                        LV::D(d, _) if is_pool_d(d) => free_d.push(d),
                        _ => {}
                    }
                }
            }
        }};
    }

    // ---- rotated loop ----------------------------------------------------------------------
    emit_pass!(0..plan.cond_len, exit_a, Vec::new());
    a.bind(body_l);
    emit_pass!(
        plan.cond_len..plan.chain.len(),
        exit_b,
        cond_virgins.clone()
    );
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
            } else {
                a.str_d_imm(d, 22, s.off + 8);
            }
        }
    };
    a.bind(exit_a);
    emit_flush(a, &cond_virgins);
    if plan.uses_ext {
        restore_ext(a);
    }
    a.b(pc_labels[plan.exit_pc]);
    a.bind(exit_b);
    emit_flush(a, &all_virgins);
    if plan.uses_ext {
        restore_ext(a);
    }
    a.b(pc_labels[plan.exit_pc]);
    if plan.uses_ext {
        a.bind(pre_fail);
        restore_ext(a);
        a.b(plain_h);
    }

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
            a.add_imm(20, 20, 16);
        }
        emit_flush(a, &seen);
        if plan.uses_ext {
            restore_ext(a);
        }
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
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_exec(a: &mut asm::Asm, pc: u32, l_unwind: usize) {
    emit_op_helper(a, H_EXEC, pc, l_unwind);
}

/// [`emit_exec`] through a DEDICATED helper slot (same `(ctx, pc, sp) → SpFlag` contract):
/// hot op families skip the generic decode.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_op_helper(a: &mut asm::Asm, idx: usize, pc: u32, l_unwind: usize) {
    a.mov(0, 19);
    a.movz(1, pc, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (idx * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
    a.cbnz(1, false, l_unwind);
}

/// An infallible helper (returns the new sp): return/handler bookkeeping.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_helper(a: &mut asm::Asm, idx: usize, imm: u32) {
    a.mov(0, 19);
    a.movz(1, imm, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (idx * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
}

/// Condition helper: leaves the flag in w1, new sp in x0 (null = threw during ToBoolean).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_cond(a: &mut asm::Asm, mode: u32, l_unwind: usize) {
    a.mov(0, 19);
    a.movz(1, mode, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (H_COND * 8) as u32);
    a.blr(16);
    a.cbz(0, true, l_unwind);
    a.mov(20, 0);
}

/// Non-owning conditional check for the short-circuit peek ops. ToBoolean cannot throw and the
/// value stays on the operand stack, so common tags need neither refcount traffic nor a helper
/// transition. BigInt and a possible HTMLDDA object retain the canonical helper path.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn emit_peek_cond_inline(
    a: &mut asm::Asm,
    layout: &crate::value::JitLayout,
    not_nullish: bool,
    l_unwind: usize,
) {
    let done = a.new_label();
    let l_false = a.new_label();
    a.ldurb(9, 20, -16);
    if not_nullish {
        // Empty is an internal completion marker, not nullish; preserve the helper's exact
        // `Undefined | Null` predicate even though Empty should not escape onto this stack.
        a.cmp_imm_w(9, 0);
        a.b_cond(C_EQ, l_false);
        a.cmp_imm_w(9, 2);
        a.cset_w(1, C_NE);
        a.b(done);
        a.bind(l_false);
        a.movz(1, 0, 0);
        a.bind(done);
        return;
    }

    let slow = a.new_label();
    let l_bool = a.new_label();
    let l_num = a.new_label();
    let l_str = a.new_label();
    let l_obj = a.new_label();
    let l_true = a.new_label();
    a.cmp_imm_w(9, 2);
    a.b_cond(C_LS, l_false);
    a.cmp_imm_w(9, 3);
    a.b_cond(C_EQ, l_bool);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_EQ, l_num);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow); // BigInt: inspect its arbitrary-precision payload in Rust.
    a.cmp_imm_w(9, 6);
    a.b_cond(C_EQ, l_str);
    a.cmp_imm_w(9, 7);
    a.b_cond(C_EQ, l_true); // Symbol
    a.cmp_imm_w(9, 8);
    a.b_cond(C_EQ, l_obj);
    a.b(slow);

    a.bind(l_bool);
    a.ldurb(1, 20, -15);
    a.b(done);
    a.bind(l_num);
    a.ldur_d(0, 20, -8);
    a.movz(12, 0, 0);
    a.fmov_d_x(1, 12);
    a.fcmp(0, 1);
    a.cset_w(11, C_EQ);
    a.cset_w(12, C_VS);
    a.logic_w(1, 11, 11, 12); // zero or NaN = falsy
    a.movz(12, 1, 0);
    a.logic_w(2, 1, 11, 12); // invert to truthy
    a.b(done);
    a.bind(l_str);
    a.ldur(12, 20, -8);
    a.ldr_w_imm(11, 12, crate::lstr::LEN_OFF as u32);
    a.cmp_imm_w(11, 0);
    a.cset_w(1, C_NE);
    a.b(done);
    a.bind(l_obj);
    a.ldur(12, 20, -8);
    a.add_imm(11, 12, layout.obj_from_rc as u32);
    a.ldrb_imm(11, 11, layout.obj_ic_plain as u32);
    a.cbz(11, false, slow); // includes the engine's possible HTMLDDA object
    a.bind(l_true);
    a.movz(1, 1, 0);
    a.b(done);
    a.bind(l_false);
    a.movz(1, 0, 0);
    a.b(done);
    a.bind(slow);
    emit_cond(a, COND_PEEK_TRUTHY, l_unwind);
    a.bind(done);
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// `ctx.global_body` for a fresh frame: the live global object's body pointer. Populated even
/// when this chunk never reads it, because a direct (shared-ctx) JIT→JIT call can only enter a
/// `needs_global` CALLEE if the caller's ctx already carries the pointer — a null here forces
/// every such call through the layered path. Falls back to null (never needed) or the original
/// panicking borrow (needed, but the global is mutably borrowed — same failure as before).
#[cfg(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn jit_global_body(i: &Interp, code: &JitCode) -> *const u8 {
    if let Ok(b) = i.global.try_borrow() {
        &*b as *const crate::value::Object as *const u8
    } else if code.needs_global {
        let b = i.global.borrow();
        &*b as *const crate::value::Object as *const u8
    } else {
        std::ptr::null()
    }
}

/// Execute a JIT-compiled chunk: mirrors `bytecode::run` (activation env, pooled slot buffer),
/// with the operand stack in a pooled flat buffer sized by the static analysis.
#[cfg(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
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
    if let Some(s) = chunk.jit_arguments_slot() {
        slots[s as usize] = Value::Obj(i.make_compiled_arguments_object(args, &env));
    }
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
        global_body: jit_global_body(i, code),
        genv: Rc::as_ptr(&i.global_env) as usize,
        interp: i as *mut Interp,
        chunk: Rc::as_ptr(chunk),
        this_val,
        slots: slots.as_mut_ptr(),
        inline_ic_safe: &i.inline_ic_safe as *const std::cell::Cell<bool> as *const u8,
        n_slots,
        slots_packed: false,
        handlers: Vec::new(),
        handler_floor: 0,
        code_base: code.mem,
        pc_offsets: code.pc_offsets.as_ptr(),
        error: None,
        ret: Value::Undefined,
    };
    ctx.this_raw = &ctx.this_val as *const Value;
    if PACKED_LOCAL_SLOTS {
        unsafe { ctx.pack_slots() };
    }
    let entry: extern "C" fn(*mut JitCtx) -> u64 = unsafe { std::mem::transmute(code.mem) };
    let ok = entry(&mut ctx);
    unsafe { ctx.unpack_slots() };
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
#[cfg(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
pub(crate) unsafe fn run_moved(
    i: &mut Interp,
    chunk: &Rc<Chunk>,
    code: &JitCode,
    env: *const Env,
    this_val: Value,
    args: *mut Value,
    argc: usize,
    // `chunk.jit_frame()`, precomputed by the caller (the cached call reads it from its IC).
    frame: (usize, usize),
) -> Result<Value, Abrupt> {
    unsafe { run_moved_inner(i, chunk, code, env, this_val, args, argc, frame, None) }
}

/// Moved-frame entry for a callee that needs a real activation environment. Captured parameter
/// values are cloned exactly once into that environment; the call's owned argument values still
/// move into the fixed frame buffer, avoiding the second full clone and both growable `Vec`s used
/// by [`run`]. An `arguments` exotic is materialized before the move and installed into its slot.
#[cfg(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
pub(crate) unsafe fn run_moved_env(
    i: &mut Interp,
    chunk: &Rc<Chunk>,
    code: &JitCode,
    definition_env: *const Env,
    this_val: Value,
    args: *mut Value,
    argc: usize,
    frame: (usize, usize),
) -> Result<Value, Abrupt> {
    let args_ref = unsafe { std::slice::from_raw_parts(args, argc) };
    let activation = chunk.jit_make_run_env(i, unsafe { &*definition_env }, &this_val, args_ref);
    let arguments = chunk.jit_arguments_slot().map(|slot| {
        (
            slot as usize,
            Value::Obj(i.make_compiled_arguments_object(args_ref, &activation)),
        )
    });
    unsafe {
        run_moved_inner(
            i,
            chunk,
            code,
            &activation as *const Env,
            this_val,
            args,
            argc,
            frame,
            arguments,
        )
    }
}

#[cfg(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
unsafe fn run_moved_inner(
    i: &mut Interp,
    chunk: &Rc<Chunk>,
    code: &JitCode,
    env: *const Env,
    this_val: Value,
    args: *mut Value,
    argc: usize,
    (n_params, n_slots): (usize, usize),
    arguments: Option<(usize, Value)>,
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
        if let Some((slot, value)) = arguments {
            if slot < seed {
                std::ptr::drop_in_place(slots_ptr.add(slot));
            }
            slots_ptr.add(slot).write(value);
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
        global_body: jit_global_body(i, code),
        genv: Rc::as_ptr(&i.global_env) as usize,
        interp: i as *mut Interp,
        chunk: Rc::as_ptr(chunk),
        this_val,
        slots: slots_ptr,
        inline_ic_safe: &i.inline_ic_safe as *const std::cell::Cell<bool> as *const u8,
        n_slots,
        slots_packed: false,
        handlers: Vec::new(),
        handler_floor: 0,
        code_base: code.mem,
        pc_offsets: code.pc_offsets.as_ptr(),
        error: None,
        ret: Value::Undefined,
    };
    ctx.this_raw = &ctx.this_val as *const Value;
    if PACKED_LOCAL_SLOTS {
        unsafe { ctx.pack_slots() };
    }
    let entry: extern "C" fn(*mut JitCtx) -> u64 = unsafe { std::mem::transmute(code.mem) };
    let ok = entry(&mut ctx);
    unsafe { ctx.unpack_slots() };
    unsafe {
        let mut p = ctx.stack_base;
        while p < ctx.final_sp {
            std::ptr::drop_in_place(p);
            p = p.add(1);
        }
        // Drop the frame's local slots (initialized Values throughout the run). Numeric frames
        // are the common case: a tag peek skips the outlined drop for trivially-copyable tags
        // (Undefined/Empty/Null/Bool/Num — repr(u8) discriminants 0..=4). Refcounted tags
        // (Str/Sym/Obj ≥ 6 — the discriminant order the templates rely on) whose payload is a
        // shared reference collapse to a bare strong-count decrement, exactly like the inline
        // templates' drop path (strong sits at payload+0 for Rc and LStr alike; BigInt tag 5
        // and last references take the real drop).
        // The bare-decrement path shares the templates' layout contract (fail closed if the
        // probe ever finds a std whose strong count moved).
        let rc_dec_ok = i
            .jit_layout
            .get()
            .is_some_and(|l| l.valid && l.rc_strong_off == 0);
        for k in 0..n_slots {
            let p = slots_ptr.add(k);
            let tag = *(p as *const u8);
            if tag < 5 {
                continue;
            }
            if rc_dec_ok && tag >= 6 {
                let strong = *(p as *const usize).add(1) as *mut usize;
                if *strong > 1 {
                    *strong -= 1;
                    continue;
                }
            }
            std::ptr::drop_in_place(p);
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

#[cfg(not(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
)))]
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
#[cfg(not(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
)))]
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

/// See the aarch64-macos definition; without machine code the fast call never commits.
#[cfg(not(any(
    all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux", target_os = "windows")),
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux", target_os = "windows"))
)))]
pub(crate) unsafe fn run_moved_env(
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
