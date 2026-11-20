//! Thin owning AST pointers backed by ordinary-aligned chunks for small nodes.
//!
//! A TLS cursor owns every unused slot in its current chunk. Handing a slot to a
//! pointer transfers one reference without changing the chunk count. Destruction
//! releases that reference atomically: coroutine handoffs can move AST ownership
//! between threads, and other nodes in the same chunk may belong to another realm.
//! Freed slots are not reused; a chunk is released after its last live/unused slot.
//! Only one unfinished chunk per size class is retained by each thread.

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::mem::{align_of, size_of, ManuallyDrop};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

const CHUNK_BYTES: usize = 8192;
const STEP: usize = align_of::<usize>();
const MAX_SLOT: usize = 256;
const CLASSES: usize = MAX_SLOT / STEP;

struct Chunk {
    remaining: AtomicUsize,
    layout: Layout,
}

struct Cursor {
    chunk: Cell<*mut Chunk>,
    next: Cell<*mut u8>,
    unused: Cell<usize>,
}

impl Cursor {
    const fn new() -> Self {
        Self {
            chunk: Cell::new(std::ptr::null_mut()),
            next: Cell::new(std::ptr::null_mut()),
            unused: Cell::new(0),
        }
    }

    fn take(&self, stride: usize) -> *mut u8 {
        if self.unused.get() == 0 {
            let layout = Layout::from_size_align(CHUNK_BYTES, align_of::<Chunk>()).unwrap();
            let base = allocate(layout);
            let offset = (size_of::<Chunk>() + STEP - 1) & !(STEP - 1);
            let slots = (CHUNK_BYTES - offset) / stride;
            debug_assert!(slots > 0);
            let chunk = base.cast::<Chunk>();
            // The chunk is unpublished and the allocation has its required alignment.
            unsafe {
                chunk.write(Chunk {
                    remaining: AtomicUsize::new(slots),
                    layout,
                });
                self.next.set(base.add(offset));
            }
            self.chunk.set(chunk);
            self.unused.set(slots);
        }
        let slot = self.next.get();
        let chunk = self.chunk.get();
        let unused = self.unused.get() - 1;
        // Each slot starts on STEP alignment and reserves a chunk-owner pointer.
        // The unused reference transferred to this slot keeps the chunk alive.
        unsafe { slot.cast::<*mut Chunk>().write(chunk) };
        self.unused.set(unused);
        if unused == 0 {
            self.next.set(std::ptr::null_mut());
            self.chunk.set(std::ptr::null_mut());
        } else {
            // There is another complete slot inside the allocation.
            self.next.set(unsafe { slot.add(stride) });
        }
        slot
    }

    fn drain(&self) {
        let unused = self.unused.replace(0);
        let chunk = self.chunk.replace(std::ptr::null_mut());
        self.next.set(std::ptr::null_mut());
        if unused != 0 {
            // These references belong exclusively to this cursor. Live pointers
            // retain their own references independently of the allocating thread.
            unsafe { release_chunk(chunk, unused) };
        }
    }
}

struct Cache {
    cursors: [Cursor; CLASSES],
    pooled: Cell<bool>,
}

impl Cache {
    fn drain(&self) {
        for cursor in &self.cursors {
            cursor.drain();
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.drain();
    }
}

thread_local! {
    static CACHE: Cache = const { Cache {
        cursors: [const { Cursor::new() }; CLASSES],
        pooled: Cell::new(true),
    } };
}

/// Small independent parses should not pin shared chunks when their functions outlive
/// neighboring parses. Larger inputs amortize chunk storage across many AST nodes.
pub(crate) struct AllocationScope {
    previous: Option<bool>,
    // Restoring a TLS policy must happen on the thread that installed it.
    thread: PhantomData<std::rc::Rc<()>>,
}

impl AllocationScope {
    pub(crate) fn for_source(bytes: usize) -> Self {
        Self {
            previous: CACHE
                .try_with(|cache| cache.pooled.replace(bytes >= 4096))
                .ok(),
            thread: PhantomData,
        }
    }
}

impl Drop for AllocationScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            let _ = CACHE.try_with(|cache| cache.pooled.set(previous));
        }
    }
}

/// Release unused slots; existing AST nodes remain valid.
#[cfg(test)]
pub(crate) fn trim() {
    let _ = CACHE.try_with(Cache::drain);
}

fn allocate(layout: Layout) -> *mut u8 {
    // Use the embedder's global allocator, including its accounting/limits.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        handle_alloc_error(layout);
    }
    base
}

unsafe fn free(base: *mut u8, layout: Layout) {
    // The caller owns the final reference to this exact allocation/layout.
    unsafe { dealloc(base, layout) };
}

unsafe fn release_chunk(chunk: *mut Chunk, count: usize) {
    // The caller owns `count` references, so the header remains live through this
    // decrement. Only the final releaser reads the immutable layout and frees it.
    let previous = unsafe { (*chunk).remaining.fetch_sub(count, Ordering::AcqRel) };
    debug_assert!(previous >= count);
    if previous == count {
        let layout = unsafe { (*chunk).layout };
        unsafe { free(chunk.cast(), layout) };
    }
}

fn slot_layout<T>() -> (Layout, usize) {
    let (layout, offset) = Layout::new::<*mut Chunk>()
        .extend(Layout::new::<T>())
        .expect("AST allocation is too large");
    (layout.pad_to_align(), offset)
}

unsafe fn release_slot<T>(ptr: NonNull<T>) {
    let (layout, offset) = slot_layout::<T>();
    // Every allocation stores its owner pointer at the allocation/slot start,
    // including standalone allocations with a null owner and arbitrary alignment.
    let base = unsafe { ptr.as_ptr().cast::<u8>().sub(offset) };
    let chunk = unsafe { base.cast::<*mut Chunk>().read() };
    if chunk.is_null() {
        unsafe { free(base, layout) };
    } else {
        unsafe { release_chunk(chunk, 1) };
    }
}

#[repr(transparent)]
pub struct AstBox<T> {
    ptr: NonNull<T>,
    owns: PhantomData<T>,
}

// Each pointer owns its T exclusively. Chunk headers are immutable except for
// their atomic lifetime count; remote destruction never accesses a TLS cursor or
// another slot. These bounds match ordinary Box ownership, including handoffs.
unsafe impl<T: Send> Send for AstBox<T> {}
unsafe impl<T: Sync> Sync for AstBox<T> {}

impl<T> AstBox<T> {
    pub fn new(value: T) -> Self {
        let (layout, offset) = slot_layout::<T>();
        let pooled = size_of::<T>() != 0 && layout.align() <= STEP && layout.size() <= MAX_SLOT;
        let slot = if pooled {
            let class = layout.size() / STEP - 1;
            CACHE
                .try_with(|cache| {
                    cache
                        .pooled
                        .get()
                        .then(|| cache.cursors[class].take(layout.size()))
                })
                .ok()
                .flatten()
        } else {
            None
        };
        let slot = slot.unwrap_or_else(|| {
            // Large/over-aligned values and allocations after TLS destruction use
            // a standalone block, recognized by its null chunk owner on release.
            let base = allocate(layout);
            unsafe { base.cast::<*mut Chunk>().write(std::ptr::null_mut()) };
            base
        });
        let ptr = unsafe { slot.add(offset).cast::<T>() };
        // Both paths provide space/alignment for T, including an aligned ZST.
        unsafe { ptr.write(value) };
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            owns: PhantomData,
        }
    }

    pub fn into_inner(this: Self) -> T {
        let this = ManuallyDrop::new(this);
        // Transfer T out before releasing only its storage, without running Drop.
        unsafe {
            let value = this.ptr.as_ptr().read();
            release_slot(this.ptr);
            value
        }
    }
}

impl<T> Drop for AstBox<T> {
    fn drop(&mut self) {
        struct Release<T>(NonNull<T>);
        impl<T> Drop for Release<T> {
            fn drop(&mut self) {
                unsafe { release_slot(self.0) };
            }
        }
        // The guard releases storage even if T's destructor unwinds. Keeping this
        // slot's reference until after T drops also protects nested same-chunk drops.
        let release = Release(self.ptr);
        unsafe { std::ptr::drop_in_place(self.ptr.as_ptr()) };
        drop(release);
    }
}

impl<T> Deref for AstBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for AstBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> AsRef<T> for AstBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for AstBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: Clone> Clone for AstBox<T> {
    fn clone(&self) -> Self {
        Self::new((**self).clone())
    }
}

impl<T: fmt::Debug> fmt::Debug for AstBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn finish() {
        trim();
        CACHE.with(|cache| assert!(cache.cursors.iter().all(|cursor| cursor.unused.get() == 0)));
    }

    #[test]
    fn allocation_scopes_restore_policy_after_nesting_and_unwind() {
        let _lock = TEST_LOCK.lock().unwrap();
        fn pooled<T>(value: &AstBox<T>) -> bool {
            let (_, offset) = slot_layout::<T>();
            unsafe {
                !value
                    .ptr
                    .as_ptr()
                    .cast::<u8>()
                    .sub(offset)
                    .cast::<*mut Chunk>()
                    .read()
                    .is_null()
            }
        }
        let small = AllocationScope::for_source(128);
        let standalone = AstBox::new([1u64; 6]);
        assert!(!pooled(&standalone));
        let large = {
            let _large = AllocationScope::for_source(4096);
            let value = AstBox::new([2u64; 6]);
            assert!(pooled(&value));
            value
        };
        assert!(!pooled(&AstBox::new([3u64; 6])));
        assert!(std::panic::catch_unwind(|| {
            let _large = AllocationScope::for_source(8192);
            assert!(pooled(&AstBox::new([4u64; 6])));
            panic!("expected policy unwind");
        })
        .is_err());
        assert!(!pooled(&AstBox::new([5u64; 6])));
        drop(small);
        assert!(pooled(&AstBox::new([6u64; 6])));
        // Both storage forms survive the scope that selected them and a thread handoff.
        std::thread::spawn(move || {
            assert_eq!(*standalone, [1; 6]);
            assert_eq!(*large, [2; 6]);
        })
        .join()
        .unwrap();
        finish();
    }

    #[test]
    fn values_alignment_clones_and_moves_across_chunks() {
        let _lock = TEST_LOCK.lock().unwrap();
        assert_eq!(size_of::<AstBox<u64>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<AstBox<u64>>>(), size_of::<usize>());
        let mut values: Vec<_> = (0..5000u64).map(|n| AstBox::new([n; 6])).collect();
        for (n, value) in values.iter_mut().enumerate() {
            assert_eq!(
                (value.as_ref() as *const [u64; 6] as usize) % align_of::<[u64; 6]>(),
                0
            );
            assert_eq!(**value, [n as u64; 6]);
            value[0] += 1;
        }
        let copy = values[123].clone();
        let moved = AstBox::into_inner(copy);
        assert_eq!(moved, [124, 123, 123, 123, 123, 123]);
        #[repr(align(64))]
        struct Aligned([u8; 80]);
        let aligned = AstBox::new(Aligned([7; 80]));
        assert_eq!((aligned.as_ref() as *const Aligned as usize) % 64, 0);
        assert_eq!(aligned.0, [7; 80]);
        let large = AstBox::new([9u64; 512]);
        assert_eq!(large[511], 9);
        drop((values, aligned, large));
        finish();
    }

    #[test]
    fn live_nodes_survive_cursor_drain_and_reallocation() {
        let _lock = TEST_LOCK.lock().unwrap();
        let first: Vec<_> = (0..77u32).map(AstBox::new).collect();
        trim();
        let second: Vec<_> = (0..1000u64).map(AstBox::new).collect();
        trim();
        for (n, v) in first.iter().enumerate() {
            assert_eq!(**v, n as u32);
        }
        for (n, v) in second.iter().enumerate() {
            assert_eq!(**v, n as u64);
        }
        drop((first, second));
        finish();
    }

    #[test]
    fn concurrent_destruction_after_allocating_thread_exits() {
        let _lock = TEST_LOCK.lock().unwrap();
        struct Count(Arc<AtomicUsize>);
        impl Drop for Count {
            fn drop(&mut self) {
                drop(AstBox::new([17u64; 6]));
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let counter = drops.clone();
        let mut values = std::thread::spawn(move || {
            (0..4000)
                .map(|_| AstBox::new(Count(counter.clone())))
                .collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let part = values.split_off(values.len() - 1000);
            workers.push(std::thread::spawn(move || drop(part)));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(drops.load(Ordering::SeqCst), 4000);
        drop(values);
        finish();
    }

    #[test]
    fn remote_frees_overlap_allocation_and_cursor_drain() {
        let _lock = TEST_LOCK.lock().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(u64, AstBox<[u64; 6]>)>(16);
        let worker = std::thread::spawn(move || {
            let mut seen = 0;
            for (number, value) in receiver {
                assert_eq!(*value, [number; 6]);
                seen += 1;
            }
            seen
        });
        for number in 0..10_000 {
            sender.send((number, AstBox::new([number; 6]))).unwrap();
            if number % 37 == 0 {
                trim();
            }
        }
        drop(sender);
        assert_eq!(worker.join().unwrap(), 10_000);
        finish();
    }

    #[test]
    fn storage_released_when_destructor_allocates_and_unwinds() {
        let _lock = TEST_LOCK.lock().unwrap();
        struct Panics(u64);
        impl Drop for Panics {
            fn drop(&mut self) {
                assert_eq!(self.0, 42);
                drop(AstBox::new([11u64; 6]));
                panic!("expected destructor panic");
            }
        }
        assert!(std::panic::catch_unwind(|| drop(AstBox::new(Panics(42)))).is_err());
        finish();
    }

    #[test]
    fn zero_sized_values_drop_once_and_into_inner_transfers_ownership() {
        let _lock = TEST_LOCK.lock().unwrap();
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Zero;
        impl Drop for Zero {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        let value = AstBox::new(Zero);
        let moved = AstBox::into_inner(value);
        assert_eq!(DROPS.load(Ordering::SeqCst), 0);
        drop(moved);
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
        finish();
    }

    #[test]
    fn allocations_after_cache_tls_destruction_use_standalone_storage() {
        let _lock = TEST_LOCK.lock().unwrap();
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Late;
        impl Drop for Late {
            fn drop(&mut self) {
                let value = AstBox::new(37u64);
                assert_eq!(*value, 37);
                drop(value);
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        thread_local! { static LATE: Late = const { Late }; }
        std::thread::spawn(|| {
            LATE.with(|_| {});
            drop(AstBox::new(1u64));
        })
        .join()
        .unwrap();
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
        finish();
    }
}
