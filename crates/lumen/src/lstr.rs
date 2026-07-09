//! The engine string: a thin refcounted UTF-8 buffer with spare capacity.
//!
//! `Value::Str` used to hold `Rc<str>` — immutable and exactly sized, which makes an append loop
//! (`s += x`, astring's `this.output += e`) inherently O(n²): every step materializes a fresh
//! allocation of the whole accumulated string. `LStr` is the classic engine fix (QuickJS's
//! JSString): `{strong, len, cap}` header + bytes, one thin pointer. When a string is *uniquely
//! referenced* an append writes in place (amortized by capacity doubling); shared strings copy
//! first, exactly like `Rc::make_mut`.
//!
//! The payload is a single 8-byte pointer with the strong count at offset 0 — the same shape the
//! JIT's inline templates already assume for refcounted payloads (`Rc`'s RcBox), so the machine
//! code that bumps/decrements tag-6 values is unchanged. Logical content is always `len` bytes of
//! valid UTF-8 (lone surrogates smuggled, as before — see [`crate::jstr`]); capacity beyond `len`
//! is invisible to every reader because `Deref` slices to `len`.
//!
//! Like `Rc`, `LStr` is neither `Send` nor `Sync` (non-atomic count; the engine is one thread
//! per realm).

use std::alloc::{alloc, dealloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

#[repr(C)]
struct Header {
    /// Strong count — MUST stay the first field (the JIT bumps it at payload offset 0).
    strong: Cell<usize>,
    len: Cell<u32>,
    cap: Cell<u32>,
    // `cap` bytes of UTF-8 follow.
}

/// See the module docs. `repr(transparent)`-thin: one pointer.
pub struct LStr {
    p: NonNull<Header>,
}

const HDR: usize = std::mem::size_of::<Header>();

/// Byte offset of the length within the header — the JIT's inline equality/truthiness templates
/// read `len` from machine code through the stored pointer (the strong count stays at offset 0).
pub(crate) const LEN_OFF: usize = std::mem::offset_of!(Header, len);
/// Byte offset of `cap` (which carries [`ASCII_HINT`] in its top bit) — the JIT's charCodeAt
/// intrinsic tests the hint from machine code.
pub(crate) const CAP_OFF: usize = std::mem::offset_of!(Header, cap);
/// Byte offset of the first content byte.
pub(crate) const DATA_OFF: usize = HDR;
/// Top bit of `cap`: the content is KNOWN all-ASCII (byte index == UTF-16 unit index, and every
/// byte IS its unit). Purely a hint — never set for non-ASCII content, may be clear for ASCII
/// content. Maintained by every constructor/mutator; capacity readers mask it off.
pub(crate) const ASCII_HINT: u32 = 1 << 31;

fn layout(cap: u32) -> Layout {
    Layout::from_size_align(HDR + cap as usize, std::mem::align_of::<Header>())
        .expect("string too large")
}

impl LStr {
    /// Allocate with `cap` bytes of capacity, seeding `content` (must fit).
    fn alloc(content: &str, cap: u32) -> LStr {
        debug_assert!(content.len() <= cap as usize);
        debug_assert!(cap & ASCII_HINT == 0, "capacity claims the hint bit");
        unsafe {
            let p = alloc(layout(cap)) as *mut Header;
            let p = NonNull::new(p).expect("allocation failed");
            // The hint holds for `content`; constructors that append more bytes afterwards
            // re-AND it with the extra bytes' ASCII-ness (see concat2/concat_grown).
            let hint = if content.is_ascii() { ASCII_HINT } else { 0 };
            p.as_ptr().write(Header {
                strong: Cell::new(1),
                len: Cell::new(content.len() as u32),
                cap: Cell::new(cap | hint),
            });
            let data = (p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(content.as_ptr(), data, content.len());
            LStr { p }
        }
    }

    /// Whether the content is KNOWN all-ASCII (see [`ASCII_HINT`]).
    #[inline]
    pub(crate) fn ascii_hint(&self) -> bool {
        self.hdr().cap.get() & ASCII_HINT != 0
    }

    #[inline]
    fn and_ascii(&self, extra_is_ascii: bool) {
        if !extra_is_ascii {
            let h = self.hdr();
            h.cap.set(h.cap.get() & !ASCII_HINT);
        }
    }

    /// The two halves concatenated (a single copy of each into the result).
    pub fn concat2(a: &str, b: &str) -> LStr {
        let total = a.len() + b.len();
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(a.is_ascii() && b.is_ascii());
        s
    }

    #[inline]
    fn hdr(&self) -> &Header {
        unsafe { self.p.as_ref() }
    }

    #[inline]
    fn data(&self) -> *const u8 {
        unsafe { (self.p.as_ptr() as *const u8).add(HDR) }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe {
            let bytes = std::slice::from_raw_parts(self.data(), self.hdr().len.get() as usize);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    /// Pointer identity (cache keys — same contract as `Rc::as_ptr`).
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.p.as_ptr() as *const u8
    }

    #[inline]
    pub fn ptr_eq(a: &LStr, b: &LStr) -> bool {
        a.p == b.p
    }

    #[inline]
    pub fn strong_count(&self) -> usize {
        self.hdr().strong.get()
    }

    /// Append in place when this is the ONLY reference and capacity suffices. Returns false
    /// (without modifying anything) otherwise — the caller copies. The unique-owner requirement
    /// is what makes the mutation invisible: no other handle can observe the content, and the
    /// caller must not hold a `&str` borrow of `self` across the call (enforced by `&mut self`).
    pub fn append_in_place(&mut self, x: &str) -> bool {
        let h = self.hdr();
        if h.strong.get() != 1 {
            return false;
        }
        let len = h.len.get() as usize;
        if len + x.len() > (h.cap.get() & !ASCII_HINT) as usize {
            return false;
        }
        unsafe {
            let data = (self.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(len), x.len());
        }
        h.len.set((len + x.len()) as u32);
        self.and_ascii(x.is_ascii());
        true
    }

    /// `self + x` with growth capacity: used by the fused append ops when in-place didn't apply.
    /// Doubles (at least) so a rebuilt accumulator amortizes the next appends.
    pub fn concat_grown(&self, x: &str) -> LStr {
        let need = self.as_str().len() + x.len();
        let cap = u32::try_from((need * 2).max(32))
            .unwrap_or(ASCII_HINT - 1)
            .min(ASCII_HINT - 1); // the top bit is the ASCII hint, never capacity
        let s = LStr::alloc(self.as_str(), cap.max(need as u32));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(self.as_str().len()), x.len());
        }
        s.hdr().len.set(need as u32);
        s.and_ascii(x.is_ascii());
        s
    }
}

impl Clone for LStr {
    #[inline]
    fn clone(&self) -> LStr {
        let h = self.hdr();
        h.strong.set(h.strong.get() + 1);
        LStr { p: self.p }
    }
}

impl Drop for LStr {
    #[inline]
    fn drop(&mut self) {
        let h = self.hdr();
        let s = h.strong.get();
        if s == 1 {
            let cap = h.cap.get() & !ASCII_HINT;
            unsafe { dealloc(self.p.as_ptr() as *mut u8, layout(cap)) };
        } else {
            h.strong.set(s - 1);
        }
    }
}

impl std::ops::Deref for LStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for LStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for LStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for LStr {
    fn from(s: &str) -> LStr {
        LStr::alloc(s, u32::try_from(s.len()).expect("string too large"))
    }
}

impl From<String> for LStr {
    fn from(s: String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<std::rc::Rc<str>> for LStr {
    fn from(s: std::rc::Rc<str>) -> LStr {
        LStr::from(&*s)
    }
}

impl From<&String> for LStr {
    fn from(s: &String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<char> for LStr {
    fn from(c: char) -> LStr {
        LStr::from(c.encode_utf8(&mut [0u8; 4]) as &str)
    }
}

impl PartialEq for LStr {
    fn eq(&self, other: &LStr) -> bool {
        LStr::ptr_eq(self, other) || self.as_str() == other.as_str()
    }
}
impl Eq for LStr {}

impl PartialEq<str> for LStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl std::hash::Hash for LStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl std::fmt::Display for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<&LStr> for std::rc::Rc<str> {
    fn from(s: &LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}

impl From<LStr> for std::rc::Rc<str> {
    fn from(s: LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}
