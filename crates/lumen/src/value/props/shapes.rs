//! Shape identities, prototype epochs, and common property keys.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
/// The empty-object shape: every `Props` starts here and all empty objects share it, so adding
/// the same first key to two of them lands on the same child shape.
pub(super) const SHAPE_EMPTY: u32 = 0;

/// The property-creation epoch (see [`Props::proto_flag`]): bumped whenever a marked prototype
/// mutates structurally, any `[[SetPrototypeOf]]` succeeds, or a `defineProperty` rewrites
/// attributes — every event that could shadow a creation IC's "the chain has no setter /
/// non-writable / own copy of this name" proof. Process-global and atomic, NOT thread-local:
/// generator/async bodies run JS on pooled worker threads sharing the same `Interp` (one thread
/// at a time via channel handoff, which also orders these accesses), so a bump from a worker
/// must be visible to caches validated on the main thread. Starts at 1; saturates at `u32::MAX`,
/// which no cache hit accepts — after ~4e9 invalidations the creation ICs simply turn off
/// instead of ABA-cycling.
static PROTO_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// The current creation-IC epoch. `u32::MAX` = permanently invalidated (see [`PROTO_EPOCH`]).
#[inline]
pub(crate) fn proto_epoch() -> u32 {
    PROTO_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stable address used by the ARM64 creation-IC template for the same relaxed epoch check.
#[inline]
pub(crate) fn proto_epoch_ptr() -> *const u32 {
    PROTO_EPOCH.as_ptr()
}

/// Invalidate every property-creation inline cache (see [`PROTO_EPOCH`]).
pub(crate) fn bump_proto_epoch() {
    let _ = PROTO_EPOCH.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| Some(v.saturating_add(1)),
    );
}

/// The object-shape (hidden-class) transition tree. A shape id encodes an *ordered sequence of
/// property keys* — two `Props` share an id exactly when they added the same keys in the same
/// order (attributes are NOT encoded; the inline cache re-checks accessor/writable at the slot).
/// `transitions[(parent, key)] = child` is memoized, so structurally-identical objects converge
/// on one id — which is what makes a shared per-site cache's shape compare meaningful (the flaw
/// that sank the earlier per-object version counter). A structural *removal* can't be a tree
/// transition (it doesn't extend the key sequence), so it mints a fresh unique id that no cache
/// ever holds — forcing a re-derive.
pub(super) struct ShapeTable {
    transitions: crate::fasthash::FastMap<(u32, Rc<str>), u32>,
    next: u32,
    /// This thread's id-range base (see `SHAPE_ORDINAL`).
    base: u32,
}

/// Allocates each thread's shape-id range. The table itself is thread-local (its `Rc<str>` keys
/// can't cross threads), but generator/async bodies run JS on pooled *worker* threads sharing
/// the same `Interp` and object graph — so ids minted on different threads flow through the same
/// inline caches and MUST NOT collide. Each thread takes a disjoint `ordinal << 24` range
/// (16.7M shapes per thread; the coroutine pool keeps the thread count small — past 256 threads
/// ordinals recycle, restoring the pre-partitioning collision odds rather than failing).
static SHAPE_ORDINAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

thread_local! {
    static SHAPES: RefCell<ShapeTable> = RefCell::new({
        let ord = SHAPE_ORDINAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xFF;
        let base = ord << 24;
        ShapeTable {
            transitions: Default::default(),
            next: base | 1, // low id 0 is skipped everywhere (SHAPE_EMPTY is the global 0)
            base,
        }
    });
}

impl ShapeTable {
    fn fresh(&mut self) -> u32 {
        let id = self.next;
        // Wrap within this thread's 24-bit range, skipping low-word 0 (SHAPE_EMPTY must stay
        // the empty object's id alone).
        let low = (id.wrapping_add(1)) & 0x00FF_FFFF;
        self.next = self.base | if low == 0 { 1 } else { low };
        id
    }
}

/// The child shape reached by adding `key` to shape `parent` (memoized so it is shared).
pub(super) fn shape_transition(parent: u32, key: &Rc<str>) -> u32 {
    SHAPES.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&c) = t.transitions.get(&(parent, key.clone())) {
            return c;
        }
        let child = t.fresh();
        t.transitions.insert((parent, key.clone()), child);
        child
    })
}

/// A fresh unique shape id (a structural removal / deopt — no cache should still match).
pub(super) fn shape_fresh() -> u32 {
    SHAPES.with(|t| t.borrow_mut().fresh())
}

/// Entry count up to which a `Props` runs without a hash index (linear-scan lookups, no hash
/// allocation or rehash on insert). Most objects — instance fields, cons cells, literals — stay
/// under it for their whole life.
pub(super) const INDEX_THRESHOLD: usize = 8;

thread_local! {
    /// Interned key strings for small array indices — every dense array element key "0".."63"
    /// shares one allocation per thread instead of allocating per element.
    static INDEX_KEYS: Vec<Rc<str>> = (0..64).map(|i| Rc::from(i.to_string().as_str())).collect();
    /// Interned keys for the properties every function object carries — closure creation in a
    /// hot loop would otherwise allocate each key string per closure.
    static FN_KEYS: [Rc<str>; 4] = [
        Rc::from("length"),
        Rc::from("name"),
        Rc::from("prototype"),
        Rc::from("constructor"),
    ];
    /// Shape reached by adding the intrinsic `"length"` key to an empty map. Array literals
    /// create this same one-property named map constantly.
    pub(super) static ARRAY_LENGTH_SHAPE: Cell<u32> = const { Cell::new(0) };
}

/// The property key for array index `n`, interned for small `n`.
pub(crate) fn index_key(n: usize) -> Rc<str> {
    if n < 64 {
        INDEX_KEYS.with(|k| k[n].clone())
    } else {
        Rc::from(n.to_string().as_str())
    }
}

/// Interned `"length"` / `"name"` / `"prototype"` / `"constructor"` keys (see `FN_KEYS`).
pub(crate) fn fn_key(i: usize) -> Rc<str> {
    FN_KEYS.with(|k| k[i].clone())
}
