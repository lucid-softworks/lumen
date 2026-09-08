//! Thread-local numeric totals keyed by global IDs; no embedded TLS or heap graph pointers.
use crate::jit::asm::Asm;
use std::{cell::RefCell, collections::BTreeMap};

#[derive(Default)]
struct Counts(BTreeMap<u64, [u64; 2]>);
impl Drop for Counts {
    fn drop(&mut self) {
        for (id, [miss, hit]) in &self.0 {
            eprintln!("[inline-guard-count] id={id} hit={hit} miss={miss}");
        }
    }
}
thread_local! { static COUNTS:RefCell<Counts> = RefCell::new(Counts::default()); }
unsafe extern "C" fn record(id: u64, success: u32) {
    // First visit on a coroutine thread may allocate diagnostic storage. This cannot
    // invoke JS or engine collection; IDs carry no engine-owned pointer.
    let recorded = COUNTS
        .try_with(|counts| {
            let Ok(mut counts) = counts.try_borrow_mut() else {
                return false;
            };
            let pair = counts.0.entry(id).or_default();
            let count = &mut pair[usize::from(success != 0)];
            *count = count.saturating_add(1);
            true
        })
        .unwrap_or(false);
    if !recorded {
        eprintln!("[inline-guard-count-error] id={id}");
    }
}

pub(super) fn emit(a: &mut Asm, id: u64, success: bool) {
    // 18 caller-save GPRs + LR/padding + 24 scalar FP homes = 352 aligned bytes.
    a.sub_imm(31, 31, 352);
    for reg in (0..18).step_by(2) {
        a.stp_off(reg, reg + 1, (reg * 8) as i32);
    }
    a.stp_off(30, 31, 144);
    for (slot, reg) in (0..8).chain(16..32).enumerate() {
        a.str_d_imm(reg, 31, 160 + slot as u32 * 8);
    }
    a.mov_imm64(0, id);
    a.movz(1, u32::from(success), 0);
    a.mov_imm64(16, record as *const () as usize as u64);
    a.blr(16);
    for (slot, reg) in (0..8).chain(16..32).enumerate() {
        a.ldr_d_imm(reg, 31, 160 + slot as u32 * 8);
    }
    a.ldp_off(30, 31, 144);
    for reg in (0..18).step_by(2) {
        a.ldp_off(reg, reg + 1, (reg * 8) as i32);
    }
    a.add_imm(31, 31, 352);
}

#[cfg(test)]
pub(super) fn totals(id: u64) -> [u64; 2] {
    COUNTS.with(|counts| counts.borrow().0.get(&id).copied().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::{sys, JitCode};

    #[test]
    fn emitted_counter_preserves_caller_scalar_registers_and_counts_on_new_threads() {
        let id = super::super::registry::unique_id();
        let mut a = Asm::new();
        a.stp_pre(29, 30, -16);
        a.stp_pre(19, 20, -16);
        a.mov(19, 0);
        for reg in 0..18 {
            a.mov_imm64(reg, 1000 + reg as u64);
        }
        for reg in (0..8).chain(16..32) {
            a.mov_imm64(20, 2000 + reg as u64);
            a.fmov_d_x(reg, 20);
        }
        emit(&mut a, id, true);
        emit(&mut a, id, false);
        for reg in 0..18 {
            a.str_imm(reg, 19, reg * 8);
        }
        for (slot, reg) in (0..8).chain(16..32).enumerate() {
            a.str_d_imm(reg, 19, 144 + slot as u32 * 8);
        }
        a.ldp_post(19, 20, 16);
        a.ldp_post(29, 30, 16);
        a.ret();
        let words = a.finish();
        let len = words.len() * 4;
        let mem = unsafe { sys::alloc_exec(words.as_ptr().cast(), len) };
        assert!(!mem.is_null());
        let code = JitCode {
            mem,
            len,
            pc_offsets: vec![],
            max_stack: 0,
            needs_global: false,
        };
        let run: unsafe extern "C" fn(*mut u64) = unsafe { std::mem::transmute(code.mem_ptr()) };
        let check = move || {
            let mut output = [0u64; 42];
            unsafe {
                run(output.as_mut_ptr());
            }
            for reg in 0..18 {
                assert_eq!(output[reg], 1000 + reg as u64);
            }
            for (slot, reg) in (0..8).chain(16..32).enumerate() {
                assert_eq!(output[18 + slot], 2000 + reg as u64);
            }
            assert_eq!(totals(id), [1, 1]);
        };
        check();
        std::thread::spawn(check)
            .join()
            .expect("cross-thread diagnostic execution");
        assert_eq!(totals(id), [1, 1]); // the second thread used its own counters
    }
}
