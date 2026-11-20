//! Baseline x86-64 template backend (Intel macOS, x64 Linux, and x64 Windows).
//!
//! This deliberately starts below the mature ARM64 backend: native branches remove bytecode
//! dispatch, while individual operations enter the existing checked helpers. Hot inline templates
//! can then be ported without duplicating runtime semantics or compromising deoptimization.

use super::{
    sys, JitCode, COND_PEEK_TRUTHY, COND_POP_TRUTHY, H_ADD_STRINGS, H_CALL, H_CALL_HIT, H_COND,
    H_EXEC, H_GET_PROP, H_INLINE_CLOSURE, H_NEW, H_POP_HANDLER, H_PUSH_HANDLER, H_RETURN,
    H_SET_PROP, H_UNWIND,
};
use crate::bytecode::{Chunk, Op, UpdKind};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy)]
enum PropRecv {
    This,
    Slot(u16),
    StackMethod {
        string_proto: Option<i32>,
        array: bool,
    },
}

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
    fn load_byte_r13(&mut self, disp: i32) {
        self.bytes(&[0x41, 0x0f, 0xb6, 0x85]);
        self.bytes(&disp.to_le_bytes());
    }
    fn load_byte_r15(&mut self, disp: i32) {
        self.bytes(&[0x41, 0x0f, 0xb6, 0x87]);
        self.bytes(&disp.to_le_bytes());
    }
    fn load_pair_r13(&mut self, disp: i32) {
        self.bytes(&[0x49, 0x8b, 0x85]);
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x49, 0x8b, 0x95]);
        self.bytes(&(disp + 8).to_le_bytes());
    }
    fn load_pair_r15(&mut self, disp: i32) {
        self.bytes(&[0x49, 0x8b, 0x87]);
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x49, 0x8b, 0x97]);
        self.bytes(&(disp + 8).to_le_bytes());
    }
    fn store_pair_r13(&mut self, disp: i32) {
        self.bytes(&[0x49, 0x89, 0x85]);
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x49, 0x89, 0x95]);
        self.bytes(&(disp + 8).to_le_bytes());
    }
    fn store_pair_r15(&mut self, disp: i32) {
        self.bytes(&[0x49, 0x89, 0x87]);
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x49, 0x89, 0x97]);
        self.bytes(&(disp + 8).to_le_bytes());
    }
    fn add_sp(&mut self, amount: i8) {
        self.bytes(&[
            0x49,
            0x83,
            if amount >= 0 { 0xc5 } else { 0xed },
            amount.unsigned_abs(),
        ]);
    }
    fn mov_pair_imm(&mut self, lo: u64, hi: u64) {
        self.bytes(&[0x48, 0xb8]);
        self.bytes(&lo.to_le_bytes());
        self.bytes(&[0x48, 0xba]);
        self.bytes(&hi.to_le_bytes());
    }
    fn inc_strong_rdx(&mut self, disp: i32) {
        self.bytes(&[0x48, 0xff, 0x82]);
        self.bytes(&disp.to_le_bytes());
    }
    fn guard_dec_strong_rdx(&mut self, disp: i32, slow: usize) {
        self.bytes(&[0x48, 0x8b, 0x82]); // rax=[payload+strong]
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x48, 0x83, 0xf8, 0x01]);
        self.jcc(0x86, slow); // last reference needs the real destructor
        self.bytes(&[0x48, 0xff, 0x8a]);
        self.bytes(&disp.to_le_bytes());
    }
    fn numeric_compare(&mut self, setcc: u8, reject_unordered: bool) {
        // xmm0=lhs.payload, xmm1=rhs.payload; ucomisd supplies ordered scalar flags.
        self.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x85]);
        self.bytes(&(-24i32).to_le_bytes());
        self.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x8d]);
        self.bytes(&(-8i32).to_le_bytes());
        self.bytes(&[0x66, 0x0f, 0x2e, 0xc1, 0x0f, setcc, 0xc0]); // setcc al
        if reject_unordered {
            self.bytes(&[0x0f, 0x9b, 0xc2, 0x20, 0xd0]); // setnp dl; and al,dl
        } else if setcc == 0x95 {
            self.bytes(&[0x0f, 0x9a, 0xc2, 0x08, 0xd0]); // setp dl; or al,dl (NaN !=)
        }
        self.bytes(&[
            0x0f, 0xb6, 0xc0, // movzx eax,al
            0xc1, 0xe0, 0x08, // shl eax,8 (Bool payload)
            0x83, 0xc8, 0x03, // or eax,3 (Bool tag)
            0x31, 0xd2, // xor edx,edx
        ]);
        self.store_pair_r13(-32);
        self.add_sp(-16);
    }
    /// Truncate a finite Number to i64, then use its low 32 bits for ToInt32/ToUint32.
    /// cvttsd2si returns i64::MIN on overflow/NaN; that sentinel takes the checked path.
    /// This also conservatively sends the exactly representable -2^63 to the helper.
    fn to_int32(&mut self, disp: i32, rhs: bool, slow: usize) {
        self.bytes(&[0xf2, 0x49, 0x0f, 0x2c, if rhs { 0x8d } else { 0x85 }]); // cvttsd2si rax/rcx,[r13+disp]
        self.bytes(&disp.to_le_bytes());
        self.bytes(&[0x49, 0xba]);
        self.bytes(&(i64::MIN as u64).to_le_bytes()); // r10=invalid conversion sentinel
        self.bytes(&[0x4c, 0x39, if rhs { 0xd1 } else { 0xd0 }]);
        self.jcc(0x84, slow);
    }
    fn numeric_bitop(&mut self, op: &Op, slow: usize) {
        self.to_int32(-24, false, slow);
        self.to_int32(-8, true, slow);
        match op {
            Op::BitAnd => self.bytes(&[0x21, 0xc8]),
            Op::BitOr => self.bytes(&[0x09, 0xc8]),
            Op::BitXor => self.bytes(&[0x31, 0xc8]),
            Op::Shl => self.bytes(&[0xd3, 0xe0]),
            Op::Shr => self.bytes(&[0xd3, 0xf8]),
            Op::UShr => self.bytes(&[0xd3, 0xe8]),
            _ => unreachable!(),
        }
        if matches!(op, Op::UShr) {
            // The 32-bit shift zero-extends eax, so the 64-bit signed conversion is uint32.
            self.bytes(&[0xf2, 0x48, 0x0f, 0x2a, 0xc0]);
        } else {
            self.bytes(&[0xf2, 0x0f, 0x2a, 0xc0]);
        }
        self.bytes(&[0xb8, 4, 0, 0, 0, 0x31, 0xd2]);
        self.store_pair_r13(-32);
        self.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x85]);
        self.bytes(&(-24i32).to_le_bytes());
        self.add_sp(-16);
    }
    /// Leave a truthiness flag in rdx; numeric/boolean/nullish operands need no helper.
    /// Refcounted values retain the helper's destructor and [[IsHTMLDDA]] handling.
    fn truthy(&mut self, pop: bool) {
        let boolean = self.label();
        let number = self.label();
        let falsy = self.label();
        let commit = self.label();
        let slow = self.label();
        let done = self.label();
        self.load_byte_r13(-16);
        self.bytes(&[0x83, 0xf8, 3]);
        self.jcc(0x84, boolean);
        self.bytes(&[0x83, 0xf8, 4]);
        self.jcc(0x84, number);
        self.bytes(&[0x83, 0xf8, 2]);
        self.jcc(0x86, falsy);
        self.jmp(slow);
        self.bind(boolean);
        self.load_byte_r13(-15); // Bool payload is the byte following its tag.
        self.bytes(&[0x89, 0xc2]); // edx=eax
        self.jmp(commit);
        self.bind(number);
        self.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x85]);
        self.bytes(&(-8i32).to_le_bytes()); // xmm0=number
        self.bytes(&[
            0x66, 0x0f, 0xef, 0xc9, // pxor xmm1,xmm1
            0x66, 0x0f, 0x2e, 0xc1, // ucomisd xmm0,xmm1
            0x0f, 0x95, 0xc2, // setne dl (both zero and NaN have ZF=1)
            0x0f, 0xb6, 0xd2, // movzx edx,dl
        ]);
        self.jmp(commit);
        self.bind(falsy);
        self.bytes(&[0x31, 0xd2]);
        self.bind(commit);
        if pop {
            self.add_sp(-16);
        }
        self.jmp(done);
        self.bind(slow);
        self.call_helper_pair(
            H_COND,
            if pop {
                COND_POP_TRUTHY
            } else {
                COND_PEEK_TRUTHY
            },
        );
        self.bytes(&[0x49, 0x89, 0xc5]); // r13=returned sp
        self.bind(done);
    }
    fn helper_spflag(&mut self, helper: usize, imm: u32, unwind: usize) {
        self.call_helper_pair(helper, imm);
        self.bytes(&[0x49, 0x89, 0xc5]); // r13 = rax (new sp)
        self.bytes(&[0x48, 0x85, 0xd2]); // test rdx, rdx (throw flag)
        self.jcc(0x85, unwind); // jne
    }
    /// Call a SpFlag helper with its packed pc/way argument already in r10d.
    fn helper_spflag_r10(&mut self, helper: usize, unwind: usize) {
        #[cfg(not(target_os = "windows"))]
        {
            self.bytes(&[0x4c, 0x89, 0xe7, 0x44, 0x89, 0xd6, 0x4c, 0x89, 0xea]);
            self.bytes(&[0x41, 0xff, 0x96]);
            self.bytes(&((helper * 8) as i32).to_le_bytes());
        }
        #[cfg(target_os = "windows")]
        {
            self.bytes(&[0x48, 0x83, 0xec, 0x30]); // shadow area and SpFlag result
            self.bytes(&[0x48, 0x8d, 0x4c, 0x24, 0x20]); // rcx=&result
            self.bytes(&[0x4c, 0x89, 0xe2, 0x45, 0x89, 0xd0]); // rdx=ctx; r8d=packed pc
            self.bytes(&[0x4d, 0x89, 0xe9, 0x41, 0xff, 0x96]); // r9=sp; call helper
            self.bytes(&((helper * 8) as i32).to_le_bytes());
            self.bytes(&[
                0x48, 0x8b, 0x44, 0x24, 0x20, 0x48, 0x8b, 0x54, 0x24, 0x28, 0x48, 0x83, 0xc4, 0x30,
            ]);
        }
        self.bytes(&[0x49, 0x89, 0xc5, 0x48, 0x85, 0xd2]);
        self.jcc(0x85, unwind);
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

// Load a receiver without changing any stack value or reference count.
fn emit_prop_receiver(a: &mut Asm, recv: PropRecv, slow: usize) {
    match recv {
        PropRecv::This => {
            // rax=ctx.this_raw; receiver must be Value::Obj; r10=stored Rc pointer.
            a.bytes(&[0x49, 0x8b, 0x84, 0x24, 0x30, 0, 0, 0]);
            a.bytes(&[0x80, 0x38, 0x08]);
            a.jcc(0x85, slow);
            a.bytes(&[0x4c, 0x8b, 0x50, 0x08]);
        }
        PropRecv::Slot(slot) => {
            let off = i32::from(slot) * 16;
            a.load_byte_r15(off);
            a.bytes(&[0x83, 0xf8, 0x08]);
            a.jcc(0x85, slow);
            a.bytes(&[0x4d, 0x8b, 0x97]);
            a.bytes(&(off + 8).to_le_bytes());
        }
        PropRecv::StackMethod { string_proto, .. } => {
            let object = a.label();
            let ready = a.label();
            a.cmp_byte_r13(-16, 8); // Value::Obj at the top of the operand stack.
            if let Some(proto) = string_proto {
                a.jcc(0x84, object);
                a.cmp_byte_r13(-16, 6); // Value::Str: use the active realm's prototype.
                a.jcc(0x85, slow);
                a.bytes(&[0x4d, 0x8b, 0x54, 0x24, 0x48]); // r10=[r12+72], ctx.interp
                a.bytes(&[0x4d, 0x8b, 0x92]); // r10=[r10+string_proto]
                a.bytes(&proto.to_le_bytes());
                a.jmp(ready);
            } else {
                a.jcc(0x85, slow);
            }
            a.bind(object);
            a.bytes(&[0x4d, 0x8b, 0x95]); // r10=[r13-8], stored receiver Rc pointer
            a.bytes(&(-8i32).to_le_bytes());
            a.bind(ready);
        }
    }
}

// rax addresses the property entry (or entries base with baked offsets). All fallible guards
// precede the refcount increment and stack append, so the slow path sees untouched owners.
fn emit_cloned_prop(a: &mut Asm, accessor_off: i32, value_off: i32, strong: i32, slow: usize) {
    a.bytes(&[0xf6, 0x80]);
    a.bytes(&accessor_off.to_le_bytes());
    a.bytes(&[crate::value::PROP_ACCESSOR as u8]);
    a.jcc(0x85, slow);
    a.bytes(&[0x48, 0x8b, 0x90]); // rdx=packed value
    a.bytes(&value_off.to_le_bytes());
    a.bytes(&[0x48, 0x89, 0xd0, 0x48, 0xc1, 0xe8, 0x30]); // eax=upper 16 bits
    let number = a.label();
    let object = a.label();
    let string = a.label();
    let symbol = a.label();
    let boolean = a.label();
    let scalar = a.label();
    let decoded = a.label();
    let clone_rc = a.label();
    a.bytes(&[0x3d]);
    a.bytes(&0xfff9u32.to_le_bytes());
    a.jcc(0x84, object);
    a.bytes(&[0x3d]);
    a.bytes(&0x7ff9u32.to_le_bytes());
    a.jcc(0x82, number);
    a.bytes(&[0x3d]);
    a.bytes(&0x7fffu32.to_le_bytes());
    a.jcc(0x87, number);
    for (tag, label) in [
        (0x7ffc_u32, boolean),
        (0x7ffe, string),
        (0x7fff, symbol),
        (0x7ffd, slow),
    ] {
        a.bytes(&[0x3d]);
        a.bytes(&tag.to_le_bytes());
        a.jcc(0x84, label);
    }
    a.jmp(scalar);
    a.bind(number);
    a.bytes(&[0xb8, 4, 0, 0, 0]); // Value::Num word 0
    a.jmp(decoded);
    a.bind(scalar); // Undefined, Empty and Null map to wide tags 0, 1 and 2.
    a.bytes(&[0x2d]);
    a.bytes(&0x7ff9u32.to_le_bytes());
    a.bytes(&[0x31, 0xd2]);
    a.jmp(decoded);
    a.bind(boolean);
    a.bytes(&[
        0x89, 0xd0, 0x83, 0xe0, 1, 0xc1, 0xe0, 8, 0x83, 0xc8, 3, 0x31, 0xd2,
    ]);
    a.jmp(decoded);
    for (label, tag) in [(string, 6u32), (symbol, 7), (object, 8)] {
        a.bind(label);
        a.bytes(&[0xb8]);
        a.bytes(&tag.to_le_bytes());
        a.jmp(clone_rc);
    }
    a.bind(clone_rc);
    a.bytes(&[0x48, 0xc1, 0xe2, 16, 0x48, 0xc1, 0xea, 16]); // remove packed tag
    a.inc_strong_rdx(strong);
    a.bind(decoded);
    // GetMethod keeps the original receiver below the cloned callee. Its existing
    // stack slot and reference count stay untouched, including on every guard miss.
    a.store_pair_r13(0);
    a.add_sp(16);
}

/// Compact monomorphic property load. Every cached structural fact is rechecked against
/// the live object graph; a miss reaches H_GET_PROP before stack or refcount state changes.
fn emit_prop_value(
    a: &mut Asm,
    layout: &crate::value::JitLayout,
    st: crate::bytecode::IcState,
    recv: PropRecv,
    slow: usize,
) -> bool {
    if !layout.valid
        || st.depth > 2
        || (st.depth == 2 && st.mid_ok & 1 == 0)
        || layout.entry_size.checked_mul(st.slot as usize).is_none()
    {
        return false;
    }
    let Ok(rcv) = i32::try_from(layout.obj_from_rc) else {
        return false;
    };
    let Ok(exotic) = i32::try_from(layout.obj_exotic) else {
        return false;
    };
    let Ok(plain) = i32::try_from(layout.obj_ic_plain) else {
        return false;
    };
    let Ok(shape) = i32::try_from(layout.obj_props + layout.props_shape) else {
        return false;
    };
    let Ok(proto) = i32::try_from(layout.obj_proto) else {
        return false;
    };
    let Ok(entries) = i32::try_from(layout.obj_props + layout.props_entries + layout.vec_ptr_off)
    else {
        return false;
    };
    let Ok(entries_len) =
        i32::try_from(layout.obj_props + layout.props_entries + layout.vec_len_off)
    else {
        return false;
    };
    let Ok(strong) = i32::try_from(layout.rc_strong_off) else {
        return false;
    };
    let Some(entry_base) = layout.entry_size.checked_mul(st.slot as usize) else {
        return false;
    };
    let Ok(value_off) = i32::try_from(entry_base + layout.entry_value) else {
        return false;
    };
    let Ok(accessor_off) = i32::try_from(entry_base + layout.entry_accessor) else {
        return false;
    };

    emit_prop_receiver(a, recv, slow);

    let guard_object = |a: &mut Asm, expected: u32, receiver: bool, slow: usize| {
        // r11 = Object body for the stored Rc in r10.
        a.bytes(&[0x4d, 0x89, 0xd3, 0x49, 0x81, 0xc3]);
        a.bytes(&rcv.to_le_bytes());
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&exotic.to_le_bytes());
        a.bytes(&[layout.exotic_none_tag]);
        if receiver && matches!(recv, PropRecv::StackMethod { .. }) {
            let accepted = a.label();
            a.jcc(0x84, accepted);
            if let PropRecv::StackMethod {
                string_proto,
                array,
            } = recv
            {
                // Array element changes do not move the shape. Accept an array only as a
                // non-holder for a name that cannot be an element index. String-wrapper
                // named entries are shape-pinned; length/index reads are excluded at emission.
                for tag in [
                    (array && st.depth >= 1).then_some(layout.exotic_array_tag),
                    string_proto.map(|_| layout.exotic_strwrap_tag),
                ]
                .into_iter()
                .flatten()
                {
                    a.bytes(&[0x41, 0x80, 0xbb]);
                    a.bytes(&exotic.to_le_bytes());
                    a.bytes(&[tag]);
                    a.jcc(0x84, accepted);
                }
            }
            a.jmp(slow);
            a.bind(accepted);
        } else {
            a.jcc(0x85, slow);
        }
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&plain.to_le_bytes());
        a.bytes(&[0]);
        a.jcc(0x84, slow);
        a.bytes(&[0x41, 0x81, 0xbb]);
        a.bytes(&shape.to_le_bytes());
        a.bytes(&expected.to_le_bytes());
        a.jcc(0x85, slow);
    };
    guard_object(a, st.recv_shape, true, slow);
    if st.depth >= 1 {
        a.bytes(&[0x4d, 0x8b, 0x93]); // r10=[r11+proto]
        a.bytes(&proto.to_le_bytes());
        a.bytes(&[0x4d, 0x85, 0xd2]);
        a.jcc(0x84, slow);
        guard_object(
            a,
            if st.depth == 1 {
                st.holder_shape
            } else {
                st.mid_shape
            },
            false,
            slow,
        );
    }
    if st.depth == 2 {
        a.bytes(&[0x4d, 0x8b, 0x93]);
        a.bytes(&proto.to_le_bytes());
        a.bytes(&[0x4d, 0x85, 0xd2]);
        a.jcc(0x84, slow);
        guard_object(a, st.holder_shape, false, slow);
    }

    // Recheck the live length before dereferencing a cached entry, even after shape guards.
    a.bytes(&[0x49, 0x81, 0xbb]); // cmp qword [r11+entries_len],slot
    a.bytes(&entries_len.to_le_bytes());
    a.bytes(&st.slot.to_le_bytes());
    a.jcc(0x86, slow);
    // rax=entries data; reject a live accessor descriptor, then clone its packed value.
    a.bytes(&[0x49, 0x8b, 0x83]);
    a.bytes(&entries.to_le_bytes());
    emit_cloned_prop(a, accessor_off, value_off, strong, slow);
    true
}

fn prop_probe_offsets(layout: &crate::value::JitLayout) -> Option<[i32; 11]> {
    if !layout.valid {
        return None;
    }
    let offsets = [
        layout.obj_from_rc,
        layout.obj_exotic,
        layout.obj_ic_plain,
        layout.obj_props + layout.props_shape,
        layout.obj_proto,
        layout.obj_props + layout.props_entries + layout.vec_ptr_off,
        layout.obj_props + layout.props_entries + layout.vec_len_off,
        layout.entry_size,
        layout.entry_accessor,
        layout.entry_value,
        layout.rc_strong_off,
    ];

    let mut result = [0; 11];
    for (out, input) in result.iter_mut().zip(offsets) {
        *out = i32::try_from(input).ok()?;
    }
    Some(result)
}

// Each compiled chunk pins the executable probe allocation it calls. The thread-local
// cache shares code across engines with the same measured layout; replacing the cache never
// invalidates existing chunks. The probes embed no object, realm, cache or chunk pointers.
pub(super) struct PropProbes {
    mem: *mut u8,
    len: usize,
    entries: [usize; 4],
    layout_key: ([i32; 11], [u8; 3]),
}

thread_local! {
    static PROP_PROBES: RefCell<Option<Rc<PropProbes>>> = const { RefCell::new(None) };
}

impl PropProbes {
    fn for_layout(layout: &crate::value::JitLayout) -> Option<Rc<Self>> {
        let layout_key = (
            prop_probe_offsets(layout)?,
            [
                layout.exotic_none_tag,
                layout.exotic_array_tag,
                layout.exotic_strwrap_tag,
            ],
        );
        PROP_PROBES.with(|cache| {
            if let Some(probes) = cache
                .borrow()
                .as_ref()
                .filter(|p| p.layout_key == layout_key)
            {
                return Some(probes.clone());
            }
            let mut a = Asm::new();
            let mut entries = [0; 4];
            for (key, entry) in entries.iter_mut().enumerate() {
                *entry = a.code.len();
                let recv = if key == 0 {
                    PropRecv::This
                } else {
                    PropRecv::StackMethod {
                        string_proto: (key & 1 != 0).then_some(0),
                        array: key & 2 != 0,
                    }
                };
                emit_prop_probe(&mut a, layout, recv);
            }
            let code = a.finish();
            let mem = unsafe { sys::alloc_exec(code.as_ptr(), code.len()) };
            if mem.is_null() {
                return None;
            }
            let probes = Rc::new(Self {
                mem,
                len: code.len(),
                entries,
                layout_key,
            });
            *cache.borrow_mut() = Some(probes.clone());
            Some(probes)
        })
    }
}

impl Drop for PropProbes {
    fn drop(&mut self) {
        unsafe {
            sys::free_exec(self.mem, self.len);
        }
    }
}

/// Emit a call to a shared leaf probe pinned by this JitCode. Each site supplies its receiver
/// and live cache; a failed guard returns to the existing checked property helper.
fn emit_live_prop_value(
    a: &mut Asm,
    probes: Option<&PropProbes>,
    cache: usize,
    recv: PropRecv,
    slow: usize,
) -> bool {
    let Some(probes) = probes else {
        return false;
    };
    let key = match recv {
        PropRecv::StackMethod {
            string_proto,
            array,
        } => usize::from(string_proto.is_some()) | (usize::from(array) << 1),
        _ => 0,
    };
    let target = unsafe { probes.mem.add(probes.entries[key]) };
    emit_prop_receiver(a, recv, slow);
    a.bytes(&[0x49, 0xb8]); // r8=cache
    a.bytes(&(cache as u64).to_le_bytes());
    a.bytes(&[0x48, 0xb8]); // rax=shared probe entry
    a.bytes(&(target as u64).to_le_bytes());
    a.bytes(&[0xff, 0xd0]); // internal leaf call, with no Rust or JS calls inside
    a.bytes(&[0x85, 0xc0]);
    a.jcc(0x84, slow);
    true
}

// r12-r15 retain the JIT frame registers. Only a successful probe advances r13;
// rax, rcx, rdx and r8-r11 are scratch. No safepoint or unwind occurs inside this leaf.
fn emit_prop_probe(a: &mut Asm, layout: &crate::value::JitLayout, recv: PropRecv) {
    let slow = a.label();
    use crate::bytecode::{
        IC_OFF_DEPTH, IC_OFF_HOLDER_SHAPE, IC_OFF_MID_OK, IC_OFF_MID_SHAPE, IC_OFF_RECV_SHAPE,
        IC_OFF_SLOT,
    };
    let [rcv, exotic, plain, shape, proto, entries, entries_len, entry_size, accessor, value, strong] =
        prop_probe_offsets(layout).expect("validated property probe layout");

    let probe = a.label();
    let miss = a.label();
    let done = a.label();
    // Internal leaf ABI: r8=cache, r10=receiver; preserve its pointer in rdx across misses.
    // The decoder may overwrite rdx only after the final structural guard.
    a.bytes(&[0x4c, 0x89, 0xd2]); // rdx=r10
    a.bytes(&[0xb9]); // ecx=remaining cache ways
    a.bytes(&(crate::bytecode::PROP_IC_WAYS as u32).to_le_bytes());
    a.bind(probe);
    a.bytes(&[0x49, 0x89, 0xd2]); // restore r10=receiver
    a.bytes(&[0x45, 0x0f, 0xb6, 0x88]);
    a.bytes(&IC_OFF_DEPTH.to_le_bytes());
    a.bytes(&[0x41, 0x83, 0xf9, 2]);
    a.jcc(0x87, miss);
    let depth_ok = a.label();
    a.jcc(0x85, depth_ok);
    a.bytes(&[0x41, 0xf6, 0x80]);
    a.bytes(&IC_OFF_MID_OK.to_le_bytes());
    a.bytes(&[1]);
    a.jcc(0x84, miss);
    a.bind(depth_ok);

    let guard_object = |a: &mut Asm, cache_shape: u32, receiver: bool| {
        a.bytes(&[0x4d, 0x89, 0xd3, 0x49, 0x81, 0xc3]); // r11=Object body
        a.bytes(&rcv.to_le_bytes());
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&exotic.to_le_bytes());
        a.bytes(&[layout.exotic_none_tag]);
        if let (
            true,
            PropRecv::StackMethod {
                string_proto,
                array,
            },
        ) = (receiver, recv)
        {
            let accepted = a.label();
            a.jcc(0x84, accepted);
            if array {
                let skip_array = a.label();
                a.bytes(&[0x45, 0x85, 0xc9]); // test r9d,r9d: array must be a non-holder
                a.jcc(0x84, skip_array);
                a.bytes(&[0x41, 0x80, 0xbb]);
                a.bytes(&exotic.to_le_bytes());
                a.bytes(&[layout.exotic_array_tag]);
                a.jcc(0x84, accepted);
                a.bind(skip_array);
            }
            if string_proto.is_some() {
                a.bytes(&[0x41, 0x80, 0xbb]);
                a.bytes(&exotic.to_le_bytes());
                a.bytes(&[layout.exotic_strwrap_tag]);
                a.jcc(0x84, accepted);
            }
            a.jmp(miss);
            a.bind(accepted);
        } else {
            a.jcc(0x85, miss);
        }
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&plain.to_le_bytes());
        a.bytes(&[0]);
        a.jcc(0x84, miss);
        a.bytes(&[0x41, 0x8b, 0x80]); // eax=cache shape
        a.bytes(&cache_shape.to_le_bytes());
        a.bytes(&[0x41, 0x39, 0x83]); // cmp [r11+shape],eax
        a.bytes(&shape.to_le_bytes());
        a.jcc(0x85, miss);
    };
    let follow_proto = |a: &mut Asm| {
        a.bytes(&[0x4d, 0x8b, 0x93]);
        a.bytes(&proto.to_le_bytes());
        a.bytes(&[0x4d, 0x85, 0xd2]);
        a.jcc(0x84, miss);
    };
    guard_object(a, IC_OFF_RECV_SHAPE, true);
    let holder = a.label();
    let single_hop = a.label();
    a.bytes(&[0x45, 0x85, 0xc9]);
    a.jcc(0x84, holder);
    follow_proto(a);
    a.bytes(&[0x41, 0x83, 0xf9, 1]);
    a.jcc(0x84, single_hop);
    guard_object(a, IC_OFF_MID_SHAPE, false);
    follow_proto(a);
    a.bind(single_hop);
    guard_object(a, IC_OFF_HOLDER_SHAPE, false);
    a.bind(holder);

    // A live bounds check makes slot*entry_size safe: a real Vec allocation covers that slot.
    a.bytes(&[0x45, 0x8b, 0x88]); // r9d=slot (zero extended)
    a.bytes(&IC_OFF_SLOT.to_le_bytes());
    a.bytes(&[0x4d, 0x39, 0x8b]); // cmp [r11+entries_len],r9
    a.bytes(&entries_len.to_le_bytes());
    a.jcc(0x86, miss);
    a.bytes(&[0x49, 0x8b, 0x83]); // rax=entries
    a.bytes(&entries.to_le_bytes());
    a.bytes(&[0x4d, 0x69, 0xc9]); // imul r9,r9,entry_size
    a.bytes(&entry_size.to_le_bytes());
    a.bytes(&[0x4c, 0x01, 0xc8]); // rax+=r9
    emit_cloned_prop(a, accessor, value, strong, slow);
    a.jmp(done);
    a.bind(miss);
    a.bytes(&[0x49, 0x81, 0xc0]); // next cache way
    a.bytes(&(std::mem::size_of::<crate::bytecode::IcState>() as u32).to_le_bytes());
    a.bytes(&[0xff, 0xc9]); // dec ecx
    a.jcc(0x85, probe);
    a.jmp(slow);
    a.bind(done);
    a.bytes(&[0xb8, 1, 0, 0, 0, 0xc3]); // success; return to call site
    a.bind(slow);
    a.bytes(&[0x31, 0xc0, 0xc3]); // miss, with stack and owners untouched
}

/// Read a known numeric dense mirror without boxing the receiver or calling a helper.
/// Holes, non-numeric elements, accessors and exotic receivers keep the checked path.
// On success: rax=index, rdx=dense sidecar, r11=object. All checks precede mutation.
fn emit_elem_local_guard(
    a: &mut Asm,
    layout: &crate::value::JitLayout,
    slot: u16,
    key_offset: i32,
    slow: usize,
) -> bool {
    let offsets = [
        layout.obj_from_rc,
        layout.obj_exotic,
        layout.obj_ic_plain,
        layout.obj_props + layout.props_mirror_flags,
        layout.obj_props + layout.props_elems,
        layout.dense_mirror + layout.vec_ptr_off,
        layout.dense_mirror + layout.vec_len_off,
    ];
    if !layout.valid || offsets.iter().any(|&off| off > i32::MAX as usize) {
        return false;
    }
    let [rcv, exotic, plain, flags, elems, _mirror_ptr, mirror_len] = offsets.map(|v| v as i32);
    a.cmp_byte_r13(key_offset, 4);
    a.jcc(0x85, slow);
    let off = i32::from(slot) * 16;
    a.load_byte_r15(off);
    a.bytes(&[0x83, 0xf8, 8]);
    a.jcc(0x85, slow);
    // Convert exactly to a canonical uint32 array index (0..2^32-2). Negative zero is 0.
    a.bytes(&[0xf2, 0x49, 0x0f, 0x2c, 0x85]);
    a.bytes(&(key_offset + 8).to_le_bytes());
    a.bytes(&[0xb9]);
    a.bytes(&(u32::MAX - 1).to_le_bytes());
    a.bytes(&[0x48, 0x39, 0xc8]);
    a.jcc(0x87, slow);
    a.bytes(&[0xf2, 0x48, 0x0f, 0x2a, 0xc0]); // cvtsi2sd xmm0,rax
    a.bytes(&[0x66, 0x41, 0x0f, 0x2e, 0x85]);
    a.bytes(&(key_offset + 8).to_le_bytes());
    a.jcc(0x85, slow);
    a.jcc(0x8a, slow);
    a.bytes(&[0x4d, 0x8b, 0x97]);
    a.bytes(&(off + 8).to_le_bytes()); // receiver in r10
    a.bytes(&[0x4d, 0x89, 0xd3, 0x49, 0x81, 0xc3]);
    a.bytes(&rcv.to_le_bytes()); // object in r11
    a.bytes(&[0x41, 0x0f, 0xb6, 0x8b]);
    a.bytes(&exotic.to_le_bytes());
    let plain_exotic = a.label();
    a.bytes(&[0x83, 0xf9, layout.exotic_none_tag]);
    a.jcc(0x84, plain_exotic);
    a.bytes(&[0x83, 0xf9, layout.exotic_array_tag]);
    a.jcc(0x85, slow);
    a.bind(plain_exotic);
    a.bytes(&[0x41, 0x80, 0xbb]);
    a.bytes(&plain.to_le_bytes());
    a.bytes(&[0]);
    a.jcc(0x84, slow);
    let mask = crate::value::MIRROR_OK | crate::value::MIRROR_NO_HOLES;
    a.bytes(&[0x41, 0x0f, 0xb6, 0x8b]);
    a.bytes(&flags.to_le_bytes());
    a.bytes(&[0x83, 0xe1, mask, 0x83, 0xf9, mask]);
    a.jcc(0x85, slow);
    a.bytes(&[0x49, 0x8b, 0x93]);
    a.bytes(&elems.to_le_bytes());
    a.bytes(&[0x48, 0x85, 0xd2]);
    a.jcc(0x84, slow);
    a.bytes(&[0x48, 0x3b, 0x82]);
    a.bytes(&mirror_len.to_le_bytes());
    a.jcc(0x83, slow);
    true
}

fn emit_elem_local_number(
    a: &mut Asm,
    layout: &crate::value::JitLayout,
    slot: u16,
    slow: usize,
) -> bool {
    if !emit_elem_local_guard(a, layout, slot, -16, slow) {
        return false;
    }
    let mirror_ptr = (layout.dense_mirror + layout.vec_ptr_off) as i32;
    a.bytes(&[0x48, 0x8b, 0x92]);
    a.bytes(&mirror_ptr.to_le_bytes());
    a.bytes(&[0xf2, 0x0f, 0x10, 0x04, 0xc2]); // xmm0=mirror[index]
    a.bytes(&[0xb8, 4, 0, 0, 0, 0x31, 0xd2]);
    a.store_pair_r13(-16);
    a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x85]);
    a.bytes(&(-8i32).to_le_bytes());
    true
}

fn emit_set_elem_local_number(
    a: &mut Asm,
    layout: &crate::value::JitLayout,
    slot: u16,
    keep: bool,
    slow: usize,
) -> bool {
    let offsets = [
        layout.dense_elems + layout.vec_ptr_off,
        layout.dense_elems + layout.vec_len_off,
        layout.obj_props + layout.props_entries + layout.vec_ptr_off,
        layout.obj_props + layout.props_entries + layout.vec_len_off,
        layout.entry_size,
        layout.entry_value,
        layout.obj_props + layout.props_mirror_flags,
        layout.dense_mirror + layout.vec_ptr_off,
    ];
    // The current packed Property stores numeric f64 bits directly. Other layouts
    // continue through the checked helper rather than depending on a tag guess.
    if layout.entry_accessor != layout.entry_value + 8
        || offsets.iter().any(|&off| off > i32::MAX as usize)
    {
        return false;
    }
    let [elements, elements_len, entries, entries_len, stride, value, flags, mirror] =
        offsets.map(|v| v as i32);
    if !emit_elem_local_guard(a, layout, slot, -32, slow) {
        return false;
    }
    a.cmp_byte_r13(-16, 4);
    a.jcc(0x85, slow);
    a.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x4d, 0xf8]); // xmm1=value
    a.bytes(&[0x66, 0x0f, 0x2e, 0xc9]); // ucomisd xmm1,xmm1
    a.jcc(0x8a, slow); // NaNs need canonical packing / mirror-hole handling.
    a.bytes(&[0x48, 0x3b, 0x82]); // index < elements.len
    a.bytes(&elements_len.to_le_bytes());
    a.jcc(0x83, slow);
    a.bytes(&[0x4c, 0x8b, 0x82]); // r8=elements.data
    a.bytes(&elements.to_le_bytes());
    a.bytes(&[0x45, 0x8b, 0x04, 0x80]); // r8d=elements[index]
    a.bytes(&[0x4d, 0x3b, 0x83]); // entry slot < entries.len (rejects NO_SLOT)
    a.bytes(&entries_len.to_le_bytes());
    a.jcc(0x83, slow);
    a.bytes(&[0x4d, 0x69, 0xc0]); // r8 *= entry stride
    a.bytes(&stride.to_le_bytes());
    a.bytes(&[0x4d, 0x8b, 0x93]); // r10=entries.data
    a.bytes(&entries.to_le_bytes());
    a.bytes(&[0x4d, 0x01, 0xc2]); // r10=entry
    // MIRROR_OK|NO_HOLES proves every covered property is writable numeric data.
    // No allocation, JS call or fallible guard occurs between this point and commit.
    a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x8a]);
    a.bytes(&value.to_le_bytes());
    a.bytes(&[0x48, 0x8b, 0x92]); // rdx=mirror.data
    a.bytes(&mirror.to_le_bytes());
    a.bytes(&[0xf2, 0x0f, 0x11, 0x0c, 0xc2]); // mirror[index]=value
    a.bytes(&[0x41, 0x80, 0xa3]);
    a.bytes(&flags.to_le_bytes());
    a.bytes(&[!crate::value::MIRROR_ALL_I32]);
    if keep {
        a.bytes(&[0xb8, 4, 0, 0, 0, 0x31, 0xd2]);
        a.store_pair_r13(-32);
        a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x4d, 0xe8]); // result payload at -24
        a.bytes(&[0x49, 0x83, 0xed, 0x10]);
    } else {
        a.bytes(&[0x49, 0x83, 0xed, 0x20]);
    }
    true
}

/// Probe the live call-cache ways without consuming any operand. A hit leaves the packed
/// pc/way argument in r10d. The following helper performs all observable call bookkeeping.
fn emit_call_probe(
    a: &mut Asm,
    chunk: &Chunk,
    layout: &crate::value::JitLayout,
    pc: usize,
    argc: u16,
    cache: u32,
    slow: usize,
) -> bool {
    use crate::bytecode::{CallIc, CALL_IC_EPOCH, CALL_IC_WAYS};
    let offsets = [
        layout.gc_data_off,
        std::mem::offset_of!(CallIc, callee),
        std::mem::offset_of!(CallIc, epoch),
        std::mem::offset_of!(CallIc, global_env),
        std::mem::size_of::<std::cell::Cell<CallIc>>(),
        std::mem::offset_of!(super::JitCtx, genv),
    ];
    // jit_call_hit reserves the upper pc bits for the way. Dynamic inline frames keep the
    // existing checked entry, matching the ARM backend's eligibility restriction.
    if !layout.valid
        || pc > u16::MAX as usize
        || chunk.dynamic_inline_location(pc)
        || offsets.iter().any(|&offset| offset > i32::MAX as usize)
    {
        return false;
    }
    let [data, callee, epoch, realm, stride, genv] = offsets.map(|v| v as i32);
    let callee_slot = -(i32::from(argc) + 1) * 16;
    a.cmp_byte_r13(callee_slot, 8);
    a.jcc(0x85, slow);
    a.bytes(&[0x4d, 0x8b, 0x8d]); // r9=stored callee RcBox
    a.bytes(&(callee_slot + 8).to_le_bytes());
    a.bytes(&[0x49, 0x81, 0xc1]); // convert to Rc::as_ptr identity
    a.bytes(&data.to_le_bytes());
    a.bytes(&[0x49, 0xb8]); // r8=first call-cache way (pinned by the owning Chunk)
    a.bytes(&(chunk.jit_call_cache_ptr(cache) as u64).to_le_bytes());
    a.bytes(&[0x48, 0xb8]);
    a.bytes(&(&CALL_IC_EPOCH as *const _ as u64).to_le_bytes());
    a.bytes(&[0x44, 0x8b, 0x10]); // r10d=live epoch (aligned atomic load on x64)
    a.bytes(&[0x4d, 0x8b, 0x9c, 0x24]); // r11=ctx.genv
    a.bytes(&genv.to_le_bytes());
    a.bytes(&[0x31, 0xc9]); // ecx=way index
    let probe = a.label();
    let next = a.label();
    let hit = a.label();
    a.bind(probe);
    a.bytes(&[0x4d, 0x3b, 0x88]); // callee identity
    a.bytes(&callee.to_le_bytes());
    a.jcc(0x85, next);
    a.bytes(&[0x45, 0x3b, 0x90]); // epoch
    a.bytes(&epoch.to_le_bytes());
    a.jcc(0x85, next);
    a.bytes(&[0x4d, 0x3b, 0x98]); // realm
    a.bytes(&realm.to_le_bytes());
    a.jcc(0x84, hit);
    a.bind(next);
    a.bytes(&[0x49, 0x81, 0xc0]);
    a.bytes(&stride.to_le_bytes());
    a.bytes(&[0xff, 0xc1, 0x81, 0xf9]);
    a.bytes(&(CALL_IC_WAYS as u32).to_le_bytes());
    a.jcc(0x82, probe);
    a.jmp(slow);
    a.bind(hit);
    a.bytes(&[0x41, 0x89, 0xca, 0x41, 0xc1, 0xe2, 0x10, 0x41, 0x81, 0xca]);
    a.bytes(&(pc as u32).to_le_bytes());
    true
}

pub(super) fn compile(
    chunk: &Chunk,
    layout: &crate::value::JitLayout,
    ilayout: &crate::interpreter::InterpLayout,
) -> Option<JitCode> {
    let ops = chunk.jit_ops();
    if ops.is_empty() || ops.len() > u32::MAX as usize || ops.iter().any(|o| matches!(o, Op::Await))
    {
        return None;
    }
    let max_stack = crate::jit_ir::Cfg::build(chunk).ok()?.jit_stack_capacity();
    let mut a = Asm::new();
    let prop_probes = ops
        .iter()
        .any(|op| {
            matches!(
                op,
                Op::GetMethod(..) | Op::GetPropThis(..) | Op::GetPropLocal(..)
            )
        })
        .then(|| PropProbes::for_layout(layout))
        .flatten();
    let pcs: Vec<_> = (0..ops.len()).map(|_| a.label()).collect();
    let unwind = a.label();
    let ret_ok = a.label();
    let ret_throw = a.label();
    let rc_ok = layout.valid && layout.rc_strong_off <= i32::MAX as usize;
    let rc_strong = layout.rc_strong_off as i32;

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
        0x4d, 0x8b, 0x7c, 0x24, 0x18, // r15 = [r12+24] (slots)
    ]);

    let mut pc_offsets = Vec::with_capacity(ops.len());
    for (pc, op) in ops.iter().enumerate() {
        a.bind(pcs[pc]);
        pc_offsets.push(a.code.len() as u32);
        match op {
            Op::Const(k) if chunk.jit_const_copyable(*k) => {
                let (lo, hi) = chunk.jit_const_bits(*k);
                a.mov_pair_imm(lo, hi);
                a.store_pair_r13(0);
                a.add_sp(16);
            }
            Op::Undef => {
                a.bytes(&[0x31, 0xc0, 0x31, 0xd2]); // zero both Value words
                a.store_pair_r13(0);
                a.add_sp(16);
            }
            Op::LoadLocal(slot) => {
                let slow = a.label();
                let done = a.label();
                let copy = a.label();
                let off = i32::from(*slot) * 16;
                a.load_byte_r15(off);
                a.bytes(&[0x83, 0xf8, 0x01]); // Empty is a TDZ throw
                a.jcc(0x84, slow);
                if rc_ok {
                    a.bytes(&[0x83, 0xf8, 0x05]); // compound BigInt clone stays checked
                    a.jcc(0x84, slow);
                    a.bytes(&[0x83, 0xf8, 0x06]);
                    a.jcc(0x82, copy);
                    a.load_pair_r15(off);
                    a.inc_strong_rdx(rc_strong);
                    a.jmp(done);
                    a.bind(copy);
                } else {
                    a.bytes(&[0x83, 0xf8, 0x04]);
                    a.jcc(0x87, slow);
                }
                a.load_pair_r15(off);
                a.bind(done);
                a.store_pair_r13(0);
                a.add_sp(16);
                let exit = a.label();
                a.jmp(exit);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(exit);
            }
            Op::StoreLocal(slot) => {
                let slow = a.label();
                let commit = a.label();
                let exit = a.label();
                let off = i32::from(*slot) * 16;
                a.load_byte_r15(off);
                if rc_ok {
                    a.bytes(&[0x83, 0xf8, 0x05]);
                    a.jcc(0x84, slow);
                    a.bytes(&[0x83, 0xf8, 0x06]);
                    a.jcc(0x82, commit);
                    a.load_pair_r15(off); // old payload in rdx
                    a.guard_dec_strong_rdx(rc_strong, slow);
                } else {
                    a.bytes(&[0x83, 0xf8, 0x04]);
                    a.jcc(0x87, slow);
                }
                a.bind(commit);
                a.load_pair_r13(-16); // move, including refcounted values
                a.store_pair_r15(off);
                a.add_sp(-16);
                a.jmp(exit);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(exit);
            }
            Op::Pop => {
                let slow = a.label();
                let commit = a.label();
                let exit = a.label();
                a.load_byte_r13(-16);
                if rc_ok {
                    a.bytes(&[0x83, 0xf8, 0x05]);
                    a.jcc(0x84, slow);
                    a.bytes(&[0x83, 0xf8, 0x06]);
                    a.jcc(0x82, commit);
                    a.load_pair_r13(-16);
                    a.guard_dec_strong_rdx(rc_strong, slow);
                } else {
                    a.bytes(&[0x83, 0xf8, 0x04]);
                    a.jcc(0x87, slow);
                }
                a.bind(commit);
                a.add_sp(-16);
                a.jmp(exit);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(exit);
            }
            Op::Dup => {
                let slow = a.label();
                let copy = a.label();
                let exit = a.label();
                a.load_byte_r13(-16);
                if rc_ok {
                    a.bytes(&[0x83, 0xf8, 0x05]);
                    a.jcc(0x84, slow);
                    a.bytes(&[0x83, 0xf8, 0x06]);
                    a.jcc(0x82, copy);
                    a.load_pair_r13(-16);
                    a.inc_strong_rdx(rc_strong);
                    a.jmp(copy);
                } else {
                    a.bytes(&[0x83, 0xf8, 0x04]);
                    a.jcc(0x87, slow);
                }
                a.bind(copy);
                a.load_pair_r13(-16);
                a.store_pair_r13(0);
                a.add_sp(16);
                a.jmp(exit);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(exit);
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                let slow = a.label();
                let done = a.label();
                a.cmp_byte_r13(-32, 4);
                a.jcc(0x85, slow);
                a.cmp_byte_r13(-16, 4);
                a.jcc(0x85, slow);
                a.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x85]);
                a.bytes(&(-24i32).to_le_bytes()); // xmm0=lhs
                let opcode = match op {
                    Op::Add => 0x58,
                    Op::Sub => 0x5c,
                    Op::Mul => 0x59,
                    _ => 0x5e,
                };
                a.bytes(&[0xf2, 0x41, 0x0f, opcode, 0x85]);
                a.bytes(&(-8i32).to_le_bytes()); // xmm0 op=rhs
                a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x85]);
                a.bytes(&(-24i32).to_le_bytes()); // keep lhs Number tag
                a.add_sp(-16);
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(
                    if matches!(op, Op::Add) {
                        H_ADD_STRINGS
                    } else {
                        H_EXEC
                    },
                    pc as u32,
                    unwind,
                );
                a.bind(done);
            }
            Op::Neg | Op::Plus => {
                let slow = a.label();
                let done = a.label();
                a.cmp_byte_r13(-16, 4);
                a.jcc(0x85, slow);
                if matches!(op, Op::Neg) {
                    a.bytes(&[0x48, 0xb8]);
                    a.bytes(&(1u64 << 63).to_le_bytes());
                    a.bytes(&[0x49, 0x31, 0x85]); // xor payload sign bit
                    a.bytes(&(-8i32).to_le_bytes());
                }
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::UpdateLocal(slot, kind) => {
                let slow = a.label();
                let done = a.label();
                let off = i32::from(*slot) * 16;
                a.load_byte_r15(off);
                a.bytes(&[0x83, 0xf8, 4]);
                a.jcc(0x85, slow); // TDZ, BigInt and coercion stay checked.
                if matches!(kind, UpdKind::PostInc | UpdKind::PostDec) {
                    a.load_pair_r15(off);
                    a.store_pair_r13(0);
                    a.add_sp(16);
                }
                a.bytes(&[0xf2, 0x41, 0x0f, 0x10, 0x87]);
                a.bytes(&(off + 8).to_le_bytes()); // xmm0=old
                a.bytes(&[0xb8, 1, 0, 0, 0, 0xf2, 0x0f, 0x2a, 0xc8]); // xmm1=1
                a.bytes(&[
                    0xf2,
                    0x0f,
                    match kind {
                        UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => 0x58,
                        _ => 0x5c,
                    },
                    0xc1,
                ]); // addsd/subsd xmm0,xmm1
                a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x87]);
                a.bytes(&(off + 8).to_le_bytes());
                if matches!(kind, UpdKind::PreInc | UpdKind::PreDec) {
                    a.load_pair_r15(off);
                    a.store_pair_r13(0);
                    a.add_sp(16);
                }
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::EqEq | Op::StrictEq | Op::NotEq | Op::StrictNotEq => {
                let slow = a.label();
                let done = a.label();
                a.load_byte_r13(-32);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                a.load_byte_r13(-16);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                let ne = matches!(op, Op::NotEq | Op::StrictNotEq);
                a.numeric_compare(if ne { 0x95 } else { 0x94 }, !ne);
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::Lt | Op::Gt | Op::Le | Op::Ge => {
                let slow = a.label();
                let done = a.label();
                a.load_byte_r13(-32);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                a.load_byte_r13(-16);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                let (setcc, reject_unordered) = match op {
                    Op::Lt => (0x92, true),
                    Op::Gt => (0x97, false),
                    Op::Le => (0x96, true),
                    _ => (0x93, false),
                };
                a.numeric_compare(setcc, reject_unordered);
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr => {
                let slow = a.label();
                let done = a.label();
                a.load_byte_r13(-32);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                a.load_byte_r13(-16);
                a.bytes(&[0x83, 0xf8, 0x04]);
                a.jcc(0x85, slow);
                a.numeric_bitop(op, slow);
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::BitNot => {
                let slow = a.label();
                let done = a.label();
                a.cmp_byte_r13(-16, 4);
                a.jcc(0x85, slow);
                a.to_int32(-8, false, slow);
                a.bytes(&[0xf7, 0xd0, 0xf2, 0x0f, 0x2a, 0xc0]); // not eax; cvtsi2sd xmm0,eax
                a.bytes(&[0xf2, 0x41, 0x0f, 0x11, 0x85]);
                a.bytes(&(-8i32).to_le_bytes());
                a.jmp(done);
                a.bind(slow);
                a.helper_spflag(H_EXEC, pc as u32, unwind);
                a.bind(done);
            }
            Op::Jump(target) => a.jmp(pcs[*target as usize]),
            Op::JumpIfFalse(target) => {
                a.truthy(true);
                a.bytes(&[0x48, 0x85, 0xd2]);
                a.jcc(0x84, pcs[*target as usize]);
            }
            Op::JumpIfFalsePeek(target) | Op::JumpIfTruePeek(target) => {
                a.truthy(false);
                a.bytes(&[0x48, 0x85, 0xd2]);
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
                let nullish = a.label();
                a.cmp_byte_r13(-16, 0); // Undefined
                a.jcc(0x84, nullish);
                a.cmp_byte_r13(-16, 2); // Null
                a.jcc(0x85, pcs[*target as usize]);
                a.bind(nullish);
            }
            Op::InlineGuard(_t, target) => {
                // The x64 backend keeps the wide Value ABI and uses the checked helper for
                // shared-function guards. Besides the pinned object identity, the helper checks
                // the live function, shared lexical environment, ordinary-object state, and
                // reflective frame prerequisites required by fresh-closure inlining.
                a.call_helper_ptr(H_INLINE_CLOSURE, pc as u32);
                a.bytes(&[0x48, 0x85, 0xc0]); // test rax, rax
                a.jcc(0x84, pcs[*target as usize]);
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
            Op::Call(argc, cache) | Op::CallWithThis(argc, cache) => {
                let slow = a.label();
                let done = a.label();
                if emit_call_probe(&mut a, chunk, layout, pc, *argc, *cache, slow) {
                    a.helper_spflag_r10(H_CALL_HIT, unwind);
                    a.jmp(done);
                }
                a.bind(slow);
                a.helper_spflag(H_CALL, pc as u32, unwind);
                a.bind(done);
            }
            Op::New(argc, _) => {
                a.helper_spflag(H_NEW, pc as u32 | ((*argc as u32) << 16), unwind);
            }
            Op::GetElemLocal(slot) => {
                let slow = a.label();
                let done = a.label();
                if emit_elem_local_number(&mut a, layout, *slot, slow) {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_EXEC, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_EXEC, pc as u32, unwind);
                }
            }
            Op::SetElemLocal(slot) | Op::SetElemLocalDrop(slot) => {
                let slow = a.label();
                let done = a.label();
                let keep = matches!(op, Op::SetElemLocal(_));
                if emit_set_elem_local_number(&mut a, layout, *slot, keep, slow) {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_EXEC, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_EXEC, pc as u32, unwind);
                }
            }
            Op::GetPropThis(_, cache) => {
                let slow = a.label();
                let done = a.label();
                let emitted = chunk
                    .jit_cache_preferred(*cache)
                    .is_some_and(|st| emit_prop_value(&mut a, layout, st, PropRecv::This, slow))
                    || emit_live_prop_value(
                        &mut a,
                        prop_probes.as_deref(),
                        chunk.jit_cache_ptr(*cache),
                        PropRecv::This,
                        slow,
                    );
                if emitted {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                }
            }
            Op::GetPropLocal(slot, _, cache) => {
                let slow = a.label();
                let done = a.label();
                let emitted = chunk.jit_cache_preferred(*cache).is_some_and(|st| {
                    emit_prop_value(&mut a, layout, st, PropRecv::Slot(*slot), slow)
                }) || emit_live_prop_value(
                    &mut a,
                    prop_probes.as_deref(),
                    chunk.jit_cache_ptr(*cache),
                    PropRecv::Slot(*slot),
                    slow,
                );
                if emitted {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                }
            }
            Op::GetMethod(name, cache) => {
                let slow = a.label();
                let done = a.label();
                let name = chunk.jit_name(*name);
                let non_index = !name.as_bytes().first().is_some_and(|b| b.is_ascii_digit());
                let string_proto =
                    (ilayout.valid && non_index && name != "length" && name != "description")
                        .then(|| i32::try_from(ilayout.string_proto).ok())
                        .flatten();
                let recv = PropRecv::StackMethod {
                    string_proto,
                    array: non_index,
                };
                let emitted = chunk
                    .jit_cache_preferred(*cache)
                    .is_some_and(|st| emit_prop_value(&mut a, layout, st, recv, slow))
                    || emit_live_prop_value(
                        &mut a,
                        prop_probes.as_deref(),
                        chunk.jit_cache_ptr(*cache),
                        recv,
                        slow,
                    );
                if emitted {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                }
            }
            Op::GetProp(..) => a.helper_spflag(H_GET_PROP, pc as u32, unwind),
            Op::SetProp(..)
            | Op::SetPropDrop(..)
            | Op::SetPropThisDrop(..)
            | Op::SetPropLocalDrop(..) => a.helper_spflag(H_SET_PROP, pc as u32, unwind),
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
        _prop_probes: prop_probes,
        mem,
        len,
        pc_offsets,
        max_stack,
        needs_global: ops
            .iter()
            .any(|o| matches!(o, Op::LoadName(..) | Op::LoadNameForCall(..))),
    })
}
