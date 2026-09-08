//! Named property maps and their shared dense side storage.
use crate::value::{Property, Value};
use shapes::SHAPE_EMPTY;
use std::rc::Rc;
pub(in crate::value) use storage::DenseStorage;
mod access;
mod array_builder;
mod elements;
mod mirror;
mod mutation;
mod shapes;
mod storage;
pub(crate) use shapes::{bump_proto_epoch, fn_key, index_key, proto_epoch, proto_epoch_ptr};
pub(in crate::value) use storage::DenseBuffers;
#[derive(Clone)]
pub struct Props {
    pub(in crate::value) entries: Vec<(Rc<str>, Property)>,
    /// This object serves (or once served) as some object's prototype: structural changes to it
    /// bump the global [`proto_epoch`], invalidating every property-*creation* inline cache
    /// (their fill-time chain walks proved "no hop shadows this name" — see
    /// [`crate::bytecode::IC_CREATE`]). Set by the creation-IC fill walk itself, one-way.
    pub(in crate::value) proto_flag: std::cell::Cell<bool>,
    /// Object shape (hidden class): the id encoding this map's ordered key sequence (see
    /// [`shapes::ShapeTable`]). Two `Props` share an id exactly when they added the same keys in the same
    /// order, so an inline cache that recorded (shape, slot) from one object can trust that slot
    /// on any other object of the same shape — without a key compare. Bumped to a child on
    /// new-key insert, to a fresh unique on a structural removal. Only consulted for non-exotic
    /// objects (arrays keep the key-compare path — same shape can mean different element counts).
    pub(in crate::value) shape: u32,
    /// One nullable cold-sidecar pointer shared by the optional hash index, packed elements,
    /// dense slot map and numeric mirror. Ordinary small named-property objects allocate none of
    /// it. Within the sidecar, `elems[n]` is the `entries` slot of canonical-index key `n`, or
    /// `NO_SLOT`; see `note_inserted` and `get_index`.
    pub(in crate::value) elems: DenseStorage,
    /// Raw-f64 read mirror of the dense elements. While `mirror_flags & MIRROR_OK`:
    /// `mirror.len() == elems.len()`, and for every `n`: `mirror[n]` is [`MIRROR_HOLE`] exactly
    /// when `elems[n]` names no element, else the element is a plain writable data property
    /// whose value is `Num(mirror[n])`. Element reads become one indexed load (no entry chase,
    /// no tag check), and `MIRROR_ALL_I32` lets the JIT's int loops skip the exactness guard
    /// entirely. Entries stay authoritative: fast writers dual-store through
    /// [`Props::set_index_value`]; any foreign `&mut` escape (`get_index_mut`, `get_mut` /
    /// `entry_at_mut` on an index key) invalidates the mirror instead of tracking it.
    /// [`MIRROR_OK`] | [`MIRROR_ALL_I32`] | [`MIRROR_NO_HOLES`].
    pub(in crate::value) mirror_flags: u8,
    /// Live hole count in `mirror` (descending array fills pad with holes and then fill them:
    /// `MIRROR_NO_HOLES` comes back when this returns to zero).
    pub(in crate::value) mirror_holes: u32,
    /// Some canonical-index key lives ONLY in the string-keyed map (inserted too far past the
    /// dense frontier — see `note_inserted`): `elems` coverage is no longer proof of element
    /// absence, so the dense append/pop fast paths stand down. One-way (sparse arrays are rare
    /// and stay sparse).
    pub(in crate::value) has_far: std::cell::Cell<bool>,
    /// This `Props` belongs to an `Exotic::Array` object: canonical-index key inserts skip the
    /// shape transition. Array shapes encode the *named*-key sequence only (matching
    /// `push_dense`, which never transitioned) — the get-IC only ever uses an array's shape to
    /// prove a named key's ABSENCE or with a per-hit key re-check, never for bare slot trust,
    /// so elements must not churn it: a stable shape is what lets `arr.push(..)`/`arr.length`
    /// sites cache at all.
    pub(in crate::value) elem_mode: std::cell::Cell<bool>,
    /// The `entries` slot of the `"prototype"` key, or `NO_SLOT` — same memo discipline as
    /// `len_slot`. Every `new` reads the constructor's `.prototype`; function objects are
    /// ordinary maps, so this skips the scan on the construct hot path.
    pub(in crate::value) proto_slot: std::cell::Cell<u32>,
    /// The `entries` slot of the `"length"` key, or `NO_SLOT`. Array `length` can't live in the
    /// inline caches (element entries occupy slots without transitioning the shape, so a shape
    /// match doesn't pin the slot) — this memo makes the every-time re-derive a direct slot read
    /// instead of a hashed key lookup. Maintained by `insert`; any slot-shifting removal resets
    /// it (`remove` re-memoizes on the next lookup via `length_slot`).
    pub(in crate::value) len_slot: std::cell::Cell<u32>,
}

/// See [`Props::mirror`]. Bit values are chosen so the masks the JIT tests (`OK|NO_HOLES` and
/// `OK|NO_HOLES|ALL_I32`) are contiguous — encodable ARM64 logical immediates.
pub(crate) const MIRROR_OK: u8 = 1;
pub(crate) const MIRROR_NO_HOLES: u8 = 2;
/// Every non-hole mirror value is an exact i32 (bit-identical through an i32 round trip, which
/// also excludes -0.0).
pub(crate) const MIRROR_ALL_I32: u8 = 4;
/// The mirror's hole sentinel: a quiet-NaN payload no arithmetic produces. A user CAN craft
/// this exact bit pattern (typed-array punning), so the write paths refuse to mirror it — it is
/// never stored as data, which is what makes reading it back as "absent" sound.
pub(crate) const MIRROR_HOLE: u64 = 0x7FF8_DEAD_0000_0001;

/// Exact-i32 (and not -0.0): the value survives an i32 round trip bit-identically.
#[inline]
pub(crate) fn f64_exact_i32(f: f64) -> bool {
    (f as i32 as f64).to_bits() == f.to_bits()
}

/// `elems` hole marker (also caps how many entries dense slots can address).
pub(super) const NO_SLOT: u32 = u32::MAX;

impl std::fmt::Debug for Props {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Props")
            .field("entries", &self.entries.len())
            .field("shape", &self.shape)
            .finish()
    }
}

impl Default for Props {
    fn default() -> Self {
        Self::new()
    }
}

impl Props {
    pub(crate) fn new() -> Props {
        Self::with_capacity(0)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Props {
        Self::with_entries(Vec::with_capacity(capacity))
    }

    pub(super) fn with_entries(entries: Vec<(Rc<str>, Property)>) -> Props {
        debug_assert!(entries.is_empty());
        Props {
            entries,
            shape: SHAPE_EMPTY,
            elems: DenseStorage::default(),
            mirror_flags: MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES,
            mirror_holes: 0,
            proto_flag: std::cell::Cell::new(false),
            has_far: std::cell::Cell::new(false),
            elem_mode: std::cell::Cell::new(false),
            proto_slot: std::cell::Cell::new(NO_SLOT),
            len_slot: std::cell::Cell::new(NO_SLOT),
        }
    }

    /// Instantiate a compiler-proved plain-data object template with its final values.
    ///
    /// Cloning the whole template would clone every placeholder [`crate::value::PackedValue`] and then drop it
    /// again as the caller overwrote each slot. Object-heavy parsers do this millions of times.
    /// The key/shape and lookup sidecars are the reusable part; plain property descriptors are
    /// cheaper and safer to construct directly around the moved values.
    pub(crate) fn instantiate_plain<I>(&self, mut values: I) -> Props
    where
        I: ExactSizeIterator<Item = Value>,
    {
        assert_eq!(values.len(), self.entries.len(), "object-template arity");
        let mut entries = Vec::with_capacity(self.entries.len());
        for (key, _) in &self.entries {
            entries.push((
                key.clone(),
                Property::plain(values.next().expect("object-template value")),
            ));
        }
        debug_assert!(values.next().is_none());
        Props {
            entries,
            proto_flag: std::cell::Cell::new(false),
            shape: self.shape,
            elems: self.elems.clone(),
            mirror_flags: self.mirror_flags,
            mirror_holes: self.mirror_holes,
            has_far: std::cell::Cell::new(self.has_far.get()),
            elem_mode: std::cell::Cell::new(self.elem_mode.get()),
            proto_slot: std::cell::Cell::new(self.proto_slot.get()),
            len_slot: std::cell::Cell::new(self.len_slot.get()),
        }
    }

    /// Grow tiny property maps exactly: `Vec`'s default first allocation has room for four
    /// 40-byte entries, while one- and two-property objects dominate real heaps. Past two entries
    /// resume geometric growth so larger maps retain amortized insertion.
    #[inline]
    pub(super) fn reserve_entry(&mut self) {
        if self.entries.len() == self.entries.capacity() {
            let additional = if self.entries.len() < 2 {
                1
            } else {
                self.entries.len()
            };
            self.entries.reserve_exact(additional);
        }
    }

    /// Mark this object as a live prototype (see `proto_flag`).
    #[inline]
    pub(crate) fn mark_proto(&self) {
        self.proto_flag.set(true);
    }

    /// Bump the creation-IC epoch if this object is a marked prototype (called by every
    /// structural mutation).
    #[inline]
    pub(super) fn note_structural(&self) {
        if self.proto_flag.get() {
            bump_proto_epoch();
        }
    }

    /// This map's shape id — the inline cache's structural validation token (see the `shape` field).
    #[inline]
    pub(crate) fn shape(&self) -> u32 {
        self.shape
    }

    /// Final named-property count of a small ordinary instance. The construct JIT records this
    /// after a successful call so forwarding constructors whose own bytecode has no direct
    /// `this.x` stores can reserve the right capacity on later allocations.
    pub(crate) fn observed_instance_capacity(&self) -> usize {
        if self.elems.0.is_none() && self.entries.len() <= 16 {
            self.entries.len()
        } else {
            0
        }
    }
}
