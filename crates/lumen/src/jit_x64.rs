//! Baseline x86-64 template backend (Intel macOS, x64 Linux, and x64 Windows).
//!
//! This deliberately starts below the mature ARM64 backend: native branches remove bytecode
//! dispatch, while individual operations enter the existing checked helpers. Hot inline templates
//! can then be ported without duplicating runtime semantics or compromising deoptimization.

use super::{
    sys, JitCode, COND_PEEK_TRUTHY, COND_POP_TRUTHY, H_ADD_STRINGS, H_CALL, H_COND, H_EXEC,
    H_GET_PROP, H_INLINE_CLOSURE, H_NEW, H_POP_HANDLER, H_PUSH_HANDLER, H_RETURN, H_SET_PROP,
    H_UNWIND,
};
use crate::bytecode::{Chunk, Op, UpdKind};

#[derive(Clone, Copy)]
enum PropRecv {
    This,
    Slot(u16),
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
    }

    let guard_object = |a: &mut Asm, expected: u32, slow: usize| {
        // r11 = Object body for the stored Rc in r10.
        a.bytes(&[0x4d, 0x89, 0xd3, 0x49, 0x81, 0xc3]);
        a.bytes(&rcv.to_le_bytes());
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&exotic.to_le_bytes());
        a.bytes(&[layout.exotic_none_tag]);
        a.jcc(0x85, slow);
        a.bytes(&[0x41, 0x80, 0xbb]);
        a.bytes(&plain.to_le_bytes());
        a.bytes(&[0]);
        a.jcc(0x84, slow);
        a.bytes(&[0x41, 0x81, 0xbb]);
        a.bytes(&shape.to_le_bytes());
        a.bytes(&expected.to_le_bytes());
        a.jcc(0x85, slow);
    };
    guard_object(a, st.recv_shape, slow);
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
            slow,
        );
    }
    if st.depth == 2 {
        a.bytes(&[0x4d, 0x8b, 0x93]);
        a.bytes(&proto.to_le_bytes());
        a.bytes(&[0x4d, 0x85, 0xd2]);
        a.jcc(0x84, slow);
        guard_object(a, st.holder_shape, slow);
    }

    // Recheck the live length before dereferencing a cached entry, even after shape guards.
    a.bytes(&[0x49, 0x81, 0xbb]); // cmp qword [r11+entries_len],slot
    a.bytes(&entries_len.to_le_bytes());
    a.bytes(&st.slot.to_le_bytes());
    a.jcc(0x86, slow);
    // rax=entries data; reject a live accessor descriptor, then clone its packed value.
    a.bytes(&[0x49, 0x8b, 0x83]);
    a.bytes(&entries.to_le_bytes());
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
    a.store_pair_r13(0);
    a.add_sp(16);
    true
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
    let max_stack = crate::jit_ir::Cfg::build(chunk).ok()?.jit_stack_capacity();
    let mut a = Asm::new();
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
            Op::Call(..) | Op::CallWithThis(..) => {
                a.helper_spflag(H_CALL, pc as u32, unwind);
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
                    .is_some_and(|st| emit_prop_value(&mut a, layout, st, PropRecv::This, slow));
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
                });
                if emitted {
                    a.jmp(done);
                    a.bind(slow);
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                    a.bind(done);
                } else {
                    a.helper_spflag(H_GET_PROP, pc as u32, unwind);
                }
            }
            Op::GetProp(..) | Op::GetMethod(..) => a.helper_spflag(H_GET_PROP, pc as u32, unwind),
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
        mem,
        len,
        pc_offsets,
        max_stack,
        needs_global: ops
            .iter()
            .any(|o| matches!(o, Op::LoadName(..) | Op::LoadNameForCall(..))),
    })
}
