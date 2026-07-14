//! Baseline x86-64 template backend (Intel macOS, x64 Linux, and x64 Windows).
//!
//! This deliberately starts below the mature ARM64 backend: native branches remove bytecode
//! dispatch, while individual operations enter the existing checked helpers. Hot inline templates
//! can then be ported without duplicating runtime semantics or compromising deoptimization.

use super::{
    stack_depths, sys, JitCode, COND_PEEK_NOT_NULLISH, COND_PEEK_TRUTHY, COND_POP_TRUTHY, H_CALL,
    H_COND, H_EXEC, H_POP_HANDLER, H_PUSH_HANDLER, H_RETURN, H_UNWIND,
};
use crate::bytecode::{Chunk, Op};

struct Asm {
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    patches: Vec<(usize, usize)>,
}

impl Asm {
    fn new() -> Self {
        Self {
            code: Vec::new(),
            labels: Vec::new(),
            patches: Vec::new(),
        }
    }
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, label: usize) {
        self.labels[label] = Some(self.code.len());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.code.extend_from_slice(bytes);
    }
    fn rel32(&mut self, label: usize) {
        self.patches.push((self.code.len(), label));
        self.code.extend_from_slice(&[0; 4]);
    }
    fn jmp(&mut self, label: usize) {
        self.code.push(0xe9);
        self.rel32(label);
    }
    fn jcc(&mut self, cc: u8, label: usize) {
        self.bytes(&[0x0f, cc]);
        self.rel32(label);
    }
    #[cfg(not(target_os = "windows"))]
    fn call_helper_pair(&mut self, helper: usize, imm: u32) {
        // rdi=ctx(r12), esi=imm, rdx=sp(r13); call [helpers(r14)+index*8].
        self.bytes(&[0x4c, 0x89, 0xe7, 0xbe]);
        self.bytes(&imm.to_le_bytes());
        self.bytes(&[0x4c, 0x89, 0xea, 0x41, 0xff, 0x96]);
        self.bytes(&((helper * 8) as i32).to_le_bytes());
    }
    #[cfg(not(target_os = "windows"))]
    fn call_helper_ptr(&mut self, helper: usize, imm: u32) {
        self.call_helper_pair(helper, imm);
    }
    #[cfg(target_os = "windows")]
    fn call_helper_pair(&mut self, helper: usize, imm: u32) {
        // Win64 returns the 16-byte SpFlag through a hidden pointer. Reserve the mandatory
        // 32-byte shadow area plus the result, preserving 16-byte alignment around the call.
        self.bytes(&[0x48, 0x83, 0xec, 0x30]); // sub rsp,48
        self.bytes(&[0x48, 0x8d, 0x4c, 0x24, 0x20]); // rcx=&result
        self.bytes(&[0x4c, 0x89, 0xe2, 0x41, 0xb8]); // rdx=ctx; r8d=imm
        self.bytes(&imm.to_le_bytes());
        self.bytes(&[0x4d, 0x89, 0xe9, 0x41, 0xff, 0x96]); // r9=sp; call helper
        self.bytes(&((helper * 8) as i32).to_le_bytes());
        self.bytes(&[
            0x48, 0x8b, 0x44, 0x24, 0x20, // rax=result.sp
            0x48, 0x8b, 0x54, 0x24, 0x28, // rdx=result.flag
            0x48, 0x83, 0xc4, 0x30, // add rsp,48
        ]);
    }
    #[cfg(target_os = "windows")]
    fn call_helper_ptr(&mut self, helper: usize, imm: u32) {
        self.bytes(&[0x48, 0x83, 0xec, 0x20]); // Win64 shadow space
        self.bytes(&[0x4c, 0x89, 0xe1, 0xba]); // rcx=ctx; edx=imm
        self.bytes(&imm.to_le_bytes());
        self.bytes(&[0x4d, 0x89, 0xe8, 0x41, 0xff, 0x96]); // r8=sp; call helper
        self.bytes(&((helper * 8) as i32).to_le_bytes());
        self.bytes(&[0x48, 0x83, 0xc4, 0x20]);
    }
    fn cmp_byte_r13(&mut self, disp: i32, imm: u8) {
        self.bytes(&[0x41, 0x80, 0xbd]);
        self.bytes(&disp.to_le_bytes());
        self.code.push(imm);
    }
    fn cmp_qword_r13_rax(&mut self, disp: i32) {
        self.bytes(&[0x49, 0x39, 0x85]);
        self.bytes(&disp.to_le_bytes());
    }
    fn helper_spflag(&mut self, helper: usize, imm: u32, unwind: usize) {
        self.call_helper_pair(helper, imm);
        self.bytes(&[0x49, 0x89, 0xc5]); // r13 = rax (new sp)
        self.bytes(&[0x48, 0x85, 0xd2]); // test rdx, rdx (throw flag)
        self.jcc(0x85, unwind); // jne
    }
    fn finish(mut self) -> Vec<u8> {
        for (at, label) in self.patches {
            let target = self.labels[label].expect("unbound x64 JIT label");
            let delta = target as i64 - (at + 4) as i64;
            let delta: i32 = delta.try_into().expect("x64 JIT branch exceeds rel32");
            self.code[at..at + 4].copy_from_slice(&delta.to_le_bytes());
        }
        self.code
    }
}

pub(super) fn compile(
    chunk: &Chunk,
    layout: &crate::value::JitLayout,
    _ilayout: &crate::interpreter::InterpLayout,
) -> Option<JitCode> {
    let ops = chunk.jit_ops();
    if ops.is_empty() || ops.len() > u32::MAX as usize || ops.iter().any(|o| matches!(o, Op::Await))
    {
        return None;
    }
    let max_stack = stack_depths(chunk)?;
    let mut a = Asm::new();
    let pcs: Vec<_> = (0..ops.len()).map(|_| a.label()).collect();
    let unwind = a.label();
    let ret_ok = a.label();
    let ret_throw = a.label();

    // Preserve five registers so the stack is 16-byte aligned at every helper call.
    a.bytes(&[
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp,rsp
        0x41, 0x54, // push r12
        0x41, 0x55, // push r13
        0x41, 0x56, // push r14
        0x41, 0x57, // push r15
    ]);
    #[cfg(not(target_os = "windows"))]
    a.bytes(&[0x49, 0x89, 0xfc]); // r12 = rdi (ctx)
    #[cfg(target_os = "windows")]
    a.bytes(&[0x49, 0x89, 0xcc]); // r12 = rcx (ctx)
    a.bytes(&[
        0x4d, 0x8b, 0x34, 0x24, // r14 = [r12] (helpers)
        0x4d, 0x8b, 0x6c, 0x24, 0x08, // r13 = [r12+8] (sp)
    ]);

    let mut pc_offsets = Vec::with_capacity(ops.len());
    for (pc, op) in ops.iter().enumerate() {
        a.bind(pcs[pc]);
        pc_offsets.push(a.code.len() as u32);
        match op {
            Op::Jump(target) => a.jmp(pcs[*target as usize]),
            Op::JumpIfFalse(target) => {
                a.call_helper_pair(H_COND, COND_POP_TRUTHY);
                a.bytes(&[0x49, 0x89, 0xc5, 0x48, 0x85, 0xd2]);
                a.jcc(0x84, pcs[*target as usize]);
            }
            Op::JumpIfFalsePeek(target) | Op::JumpIfTruePeek(target) => {
                a.call_helper_pair(H_COND, COND_PEEK_TRUTHY);
                a.bytes(&[0x49, 0x89, 0xc5, 0x48, 0x85, 0xd2]);
                a.jcc(
                    if matches!(op, Op::JumpIfFalsePeek(_)) {
                        0x84
                    } else {
                        0x85
                    },
                    pcs[*target as usize],
                );
            }
            Op::JumpIfNotNullishPeek(target) => {
                a.call_helper_pair(H_COND, COND_PEEK_NOT_NULLISH);
                a.bytes(&[0x49, 0x89, 0xc5, 0x48, 0x85, 0xd2]);
                a.jcc(0x85, pcs[*target as usize]);
            }
            Op::InlineGuard(t, target) => {
                let it = chunk.jit_inline_target(*t);
                let stored = it.pin.upgrade().filter(|_| layout.valid).map(|o| {
                    let some: Option<crate::value::Gc> = Some(o);
                    unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
                });
                match stored {
                    None => a.jmp(pcs[*target as usize]),
                    Some(stored) => {
                        let callee = -((it.argc as i32 + 1) * 16);
                        a.cmp_byte_r13(callee, 8); // Value::Obj
                        a.jcc(0x85, pcs[*target as usize]);
                        a.bytes(&[0x48, 0xb8]); // movabs rax, stored Rc pointer
                        a.bytes(&(stored as u64).to_le_bytes());
                        a.cmp_qword_r13_rax(callee + 8);
                        a.jcc(0x85, pcs[*target as usize]);
                        if it.check_this {
                            a.cmp_byte_r13(callee - 16, 8);
                            a.jcc(0x85, pcs[*target as usize]);
                        }
                    }
                }
            }
            Op::Return => {
                a.call_helper_ptr(H_RETURN, 1);
                a.bytes(&[0x49, 0x89, 0xc5]);
                a.jmp(ret_ok);
            }
            Op::ReturnUndef => {
                a.call_helper_ptr(H_RETURN, 0);
                a.bytes(&[0x49, 0x89, 0xc5]);
                a.jmp(ret_ok);
            }
            Op::PushHandler(target) => {
                a.call_helper_ptr(H_PUSH_HANDLER, *target as u32);
                a.bytes(&[0x49, 0x89, 0xc5]);
            }
            Op::PopHandler => {
                a.call_helper_ptr(H_POP_HANDLER, 0);
                a.bytes(&[0x49, 0x89, 0xc5]);
            }
            Op::Throw => {
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.jmp(unwind);
            }
            Op::Call(..) | Op::CallWithThis(..) => {
                a.helper_spflag(H_CALL, pc as u32, unwind);
            }
            _ => a.helper_spflag(H_EXEC, pc as u32, unwind),
        }
    }
    a.call_helper_ptr(H_RETURN, 0);
    a.bytes(&[0x49, 0x89, 0xc5]);
    a.jmp(ret_ok);

    a.bind(unwind);
    a.call_helper_pair(H_UNWIND, 0);
    a.bytes(&[0x48, 0x85, 0xc0]); // test returned catch address
    a.jcc(0x84, ret_throw);
    a.bytes(&[0x49, 0x89, 0xd5, 0xff, 0xe0]); // r13=rdx; jmp rax

    let epilogue = |a: &mut Asm, ok: bool| {
        a.bytes(&[0x4d, 0x89, 0x6c, 0x24, 0x10]); // ctx.final_sp = r13
        if ok {
            a.bytes(&[0xb8, 1, 0, 0, 0]);
        } else {
            a.bytes(&[0x31, 0xc0]);
        }
        a.bytes(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5d, 0xc3]);
    };
    a.bind(ret_ok);
    epilogue(&mut a, true);
    a.bind(ret_throw);
    epilogue(&mut a, false);

    let code = a.finish();
    let len = code.len();
    let mem = unsafe { sys::alloc_exec(code.as_ptr(), len) };
    if mem.is_null() {
        return None;
    }
    Some(JitCode {
        mem,
        len,
        pc_offsets,
        max_stack,
        needs_global: ops
            .iter()
            .any(|o| matches!(o, Op::LoadName(..) | Op::LoadNameForCall(..))),
    })
}
