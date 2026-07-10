//! A thread-local size-class caching allocator (std-only).
//!
//! The engine's workloads are allocation-bound in exactly the way general-purpose system
//! allocators are slowest: millions of short-lived, same-sized blocks (an `RcBox<RefCell<Object>>`
//! per JS object, an `RcBox<RefCell<Scope>>` per activation, `Props` entry vectors, `LStr`
//! buffers). On macOS in particular, `malloc`/`free` pairs dominate parser-shaped profiles.
//!
//! This allocator sits in front of [`std::alloc::System`]: small blocks (≤ [`MAX_CLASS`] bytes,
//! alignment ≤ 16) are rounded up to a 16-byte size class and served from a per-thread
//! INTRUSIVE free list — a freed block's first word points at the next free block, so the
//! allocator itself never allocates (re-entrancy is what a `Vec`-backed cache dies on).
//! Every cacheable request is allocated from the system with its CLASS layout, never the
//! caller's exact layout, so a block can migrate between call sites of the same class and the
//! system layout contract still holds. Large or over-aligned requests pass straight through.
//!
//! Threads (coroutine parking) are handled by construction: each thread caches its own frees,
//! and a block freed on a different thread than it was allocated on simply joins that thread's
//! cache — the backing system allocation is thread-agnostic. Thread teardown drains the lists
//! back to the system (`Drop`); allocation during teardown falls through to the system
//! (`try_with`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Largest cached block size, in bytes.
const MAX_CLASS: usize = 1024;
/// 16-byte class granularity (also the maximum supported alignment for cached blocks).
const STEP: usize = 16;
const NUM_CLASSES: usize = MAX_CLASS / STEP;
/// Per-class cache bound. Worst case across every class is a few tens of MB per thread; in
/// practice a handful of hot classes hold a few hundred KB.
const CLASS_CAP: usize = 65536;

struct Cache {
    heads: [Cell<*mut u8>; NUM_CLASSES],
    counts: [Cell<usize>; NUM_CLASSES],
}

impl Drop for Cache {
    fn drop(&mut self) {
        for (k, head) in self.heads.iter().enumerate() {
            let layout = class_layout(k);
            let mut p = head.get();
            while !p.is_null() {
                let next = unsafe { *(p as *mut *mut u8) };
                unsafe { System.dealloc(p, layout) };
                p = next;
            }
            head.set(std::ptr::null_mut());
        }
    }
}

thread_local! {
    static CACHE: Cache = Cache {
        heads: [const { Cell::new(std::ptr::null_mut()) }; NUM_CLASSES],
        counts: [const { Cell::new(0) }; NUM_CLASSES],
    };
}

#[inline]
fn class_of(size: usize, align: usize) -> Option<usize> {
    if align <= STEP && size <= MAX_CLASS && size > 0 {
        Some((size + STEP - 1) / STEP - 1)
    } else {
        None
    }
}

#[inline]
fn class_layout(class: usize) -> Layout {
    // Size is a non-zero multiple of STEP with STEP alignment: always valid.
    unsafe { Layout::from_size_align_unchecked((class + 1) * STEP, STEP) }
}

/// See the module docs.
pub struct ClassAlloc;

unsafe impl GlobalAlloc for ClassAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(class) = class_of(layout.size(), layout.align()) {
            let cached = CACHE
                .try_with(|c| {
                    let p = c.heads[class].get();
                    if !p.is_null() {
                        c.heads[class].set(unsafe { *(p as *mut *mut u8) });
                        c.counts[class].set(c.counts[class].get() - 1);
                    }
                    p
                })
                .unwrap_or(std::ptr::null_mut());
            if !cached.is_null() {
                return cached;
            }
            return System.alloc(class_layout(class));
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(class) = class_of(layout.size(), layout.align()) {
            let cached = CACHE
                .try_with(|c| {
                    if c.counts[class].get() < CLASS_CAP {
                        unsafe { *(ptr as *mut *mut u8) = c.heads[class].get() };
                        c.heads[class].set(ptr);
                        c.counts[class].set(c.counts[class].get() + 1);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !cached {
                System.dealloc(ptr, class_layout(class));
            }
            return;
        }
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Within one size class a grow/shrink is free; otherwise allocate-copy-free through
        // the same class discipline.
        if let (Some(a), Some(b)) = (
            class_of(layout.size(), layout.align()),
            class_of(new_size, layout.align()),
        ) {
            if a == b {
                return ptr;
            }
        }
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}
