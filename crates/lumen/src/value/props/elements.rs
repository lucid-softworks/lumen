//! Dense element storage, packed array construction, and array length access.
use super::shapes::{fn_key, shape_transition, ARRAY_LENGTH_SHAPE, SHAPE_EMPTY};
use super::storage::{DenseBuffers, DenseStorage, InlinePacked, INLINE_PACKED_CAPACITY};
use super::{Props, MIRROR_HOLE, MIRROR_NO_HOLES, MIRROR_OK, NO_SLOT};
use crate::value::{index_key, Property, Value};
use std::cell::Cell;

impl Props {
    /// Construct a small dense array map directly from moved JIT stack values.
    ///
    /// # Safety
    /// `items..items+len` contains initialized `Value`s relinquished by the caller.
    pub(crate) unsafe fn packed_array_from_raw(items: *mut Value, len: usize) -> Props {
        debug_assert!(len <= 32);
        let inline = len <= INLINE_PACKED_CAPACITY;
        let mut packed = Vec::with_capacity(if inline { 0 } else { len });
        if !inline {
            for index in 0..len {
                packed.push(Property::plain(unsafe { items.add(index).read() }));
            }
        }
        let length_key = fn_key(0);
        let shape = ARRAY_LENGTH_SHAPE.with(|cached| {
            let shape = cached.get();
            if shape != 0 {
                shape
            } else {
                let shape = shape_transition(SHAPE_EMPTY, &length_key);
                cached.set(shape);
                shape
            }
        });
        Props {
            entries: vec![(
                length_key,
                Property::data(Value::Num(len as f64), true, false, false),
            )],
            shape,
            elems: DenseStorage(Some(Box::new(DenseBuffers {
                index: None,
                packed: (!inline).then(|| Box::new(packed)),
                inline_packed: if inline {
                    unsafe { InlinePacked::from_raw(items, len) }
                } else {
                    InlinePacked::default()
                },
                elems: Vec::new(),
                mirror: Vec::new(),
            }))),
            mirror_flags: 0,
            mirror_holes: 0,
            proto_flag: Cell::new(false),
            has_far: Cell::new(false),
            elem_mode: Cell::new(true),
            proto_slot: Cell::new(NO_SLOT),
            len_slot: Cell::new(0),
        }
    }

    /// Reserve the exact backing storage for a dense array whose initial length is known.
    /// `entries` needs one additional slot for the array's own `length` property. Small literals
    /// use the keyless packed representation: it avoids allocating/cloning one decimal string key
    /// per element, while all indexed/reflection paths already understand packed properties.
    /// Larger numeric arrays retain the raw-f64 mirror used by numeric JIT regions.
    pub(crate) fn reserve_dense_exact(&mut self, len: usize, numeric: bool) {
        if (1..=32).contains(&len) {
            self.entries.reserve_exact(1); // own `length`
            self.elems
                .set_packed(Some(Box::new(Vec::with_capacity(len))));
            self.mirror_flags = 0;
        } else {
            self.entries.reserve_exact(len.saturating_add(1));
            self.elems.reserve_exact(len);
        }
        if numeric && !self.elems.packed_is_some() {
            self.elems.mirror_reserve_exact(len);
        }
    }

    /// Represent a very small holey array with keyless packed property slots. `Value::Empty`
    /// remains an absent property to every reflective operation, but a later indexed write can
    /// activate the already-allocated slot without allocating an index string or growing the
    /// entry/dense vectors. Keep this deliberately tiny: an untouched `new Array(n)` must not
    /// turn a length word into an unbounded allocation, and eight slots cap the speculative
    /// footprint at 128 bytes while becoming smaller than the classic representation once filled.
    pub(crate) fn reserve_small_holes(&mut self, len: usize) {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ENABLED.get_or_init(|| std::env::var_os("LUMEN_JIT_NO_PACKED_HOLES").is_none())
            || len == 0
            || len > 8
            || self.elems.packed_is_some()
        {
            return;
        }
        debug_assert_eq!(self.elems.len(), 0);
        let mut packed = Vec::with_capacity(len);
        packed.resize_with(len, || Property::plain(Value::Empty));
        self.elems.set_packed(Some(Box::new(packed)));
        // The raw-f64 mirror describes classic `elems` slots, not keyless packed properties.
        self.mirror_flags = 0;
    }

    /// Validate the storage contract used by the numeric CFG region and return the stable boxed
    /// Vec header. The caller separately proves ordinary Array prototype semantics and keeps the
    /// owning object rooted; no helper or vector resize may run while the returned pointer lives.
    pub(crate) fn jit_packed_numeric_slots(&mut self, len: usize) -> Option<*mut Vec<Property>> {
        if len == 0 || self.proto_flag.get() || self.has_far.get() {
            return None;
        }
        let packed = self.elems.packed_mut()?;
        if packed.len() < len
            || packed[..len].iter().any(|p| {
                p.accessor() || !p.writable() || !matches!(p.value(), Value::Empty | Value::Num(_))
            })
        {
            return None;
        }
        Some(packed as *mut Vec<Property>)
    }

    /// Mark this map as an array's (see `elem_mode`). One-way, set when the owning object
    /// becomes `Exotic::Array`.
    #[inline]
    pub(crate) fn mark_array(&self) {
        self.elem_mode.set(true);
    }

    /// The `"length"` property, resolved through the `len_slot` memo (one compare, no hashing).
    /// `None` when there is no own `length`.
    pub(crate) fn length_property(&self) -> Option<&Property> {
        let s = self.len_slot.get();
        if s != NO_SLOT {
            debug_assert!(matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "length"));
            return self.entries.get(s as usize).map(|(_, p)| p);
        }
        let slot = self.find("length")?;
        self.len_slot.set(slot as u32);
        Some(&self.entries[slot].1)
    }

    /// The own property for canonical index `n`, without hashing. `None` only means "not in the
    /// dense map" — the caller must fall back to the string-keyed path, not conclude absence.
    #[inline]
    pub(crate) fn get_index(&self, n: u32) -> Option<&Property> {
        if let Some(packed) = self.elems.packed_ref() {
            return packed
                .get(n as usize)
                .filter(|p| !matches!(p.value(), Value::Empty));
        }
        let slot = *self.elems.get(n as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        Some(&self.entries[slot as usize].1)
    }

    /// Mutable [`get_index`].
    #[inline]
    pub(crate) fn get_index_mut(&mut self, n: u32) -> Option<&mut Property> {
        // A raw &mut escape can rewrite the value behind the mirror's back.
        self.mirror_invalidate();
        if self.elems.packed_is_some() {
            return self
                .elems
                .packed_mut()
                .expect("packed storage checked")
                .get_mut(n as usize)
                .filter(|p| !matches!(p.value(), Value::Empty));
        }
        let dense = self.elems.buffers_mut();
        let slot = *dense.elems.get(n as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        Some(&mut self.entries[slot as usize].1)
    }

    /// Dense tail append: insert element `n` when `n` is exactly the dense frontier and no
    /// map-only ("far") canonical key exists — which together prove the key is absent, so the
    /// whole existence scan and key-string hashing of [`Props::insert`] can be skipped. Array
    /// (`elem_mode`) maps only: the shape is untouched. Returns `false` (nothing changed) when
    /// the gates don't hold; the caller runs the generic path.
    pub(crate) fn try_append_element(&mut self, n: u32, prop: Property) -> Result<(), Property> {
        if let Some(packed) = self.elems.packed_ref() {
            if self.has_far.get() || !self.elem_mode.get() || n as usize != packed.len() {
                return Err(prop);
            }
            self.note_structural();
            self.elems.packed_mut().unwrap().push(prop);
            return Ok(());
        }
        if self.has_far.get() || !self.elem_mode.get() || n as usize != self.elems.len() {
            return Err(prop);
        }
        self.note_structural();
        let slot = self.entries.len();
        let key = index_key(n as usize);
        if let Some(index) = self.elems.index_mut() {
            index.insert(key.clone(), slot);
        }
        self.reserve_entry();
        self.entries.push((key, prop));
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
        Ok(())
    }

    /// Insert an absent canonical index directly into the classic dense map, including a bounded
    /// run of holes. The caller has already proved ordinary Array prototype semantics. This is
    /// the numeric-key counterpart of `insert`: it avoids parsing/comparing a decimal key we
    /// already know, while retaining the JIT-addressable entry/slot layout.
    pub(crate) fn try_define_dense_element(
        &mut self,
        n: u32,
        prop: Property,
    ) -> Result<(), Property> {
        if self.elems.packed_is_some() || self.has_far.get() || !self.elem_mode.get() {
            return Err(prop);
        }
        let n = n as usize;
        let old_len = self.elems.len();
        if n < old_len {
            if self.elems[n] != NO_SLOT {
                return Err(prop);
            }
        } else if n > old_len + 256 {
            return Err(prop);
        }
        self.note_structural();
        let slot = self.entries.len();
        let key = index_key(n);
        if let Some(index) = self.elems.index_mut() {
            index.insert(key.clone(), slot);
        }
        self.reserve_entry();
        self.entries.push((key, prop));
        if n < old_len {
            self.elems[n] = slot as u32;
            self.mirror_sync(n, slot, true);
        } else {
            let pads = n - old_len;
            while self.elems.len() < n {
                self.elems.push(NO_SLOT);
            }
            self.elems.push(slot as u32);
            self.mirror_grow(pads, slot);
        }
        Ok(())
    }

    pub(crate) fn append_element(&mut self, n: u32, prop: Property) -> bool {
        self.try_append_element(n, prop).is_ok()
    }

    /// Dense tail pop: remove element `n` (the array's last) when it is also the last *entry*
    /// (the common stack discipline — elements are appended last) and the last dense slot, and
    /// no "far" canonical key exists. Everything is O(1) pops: no entry shift, no re-index, no
    /// shape change (`elem_mode` maps keep their shape — element keys aren't part of it).
    /// `Some(value)` = removed; `None` = gates failed, nothing changed, caller goes generic.
    pub(crate) fn pop_last_element(&mut self, n: u32) -> Option<Value> {
        if self.has_far.get() || !self.elem_mode.get() {
            return None;
        }
        if let Some(packed) = self.elems.packed_ref() {
            if n as usize + 1 != packed.len() {
                return None;
            }
            let p = packed.last()?;
            if matches!(p.value(), Value::Empty) || p.accessor() || !p.configurable() {
                return None;
            }
            self.note_structural();
            return self
                .elems
                .packed_mut()
                .unwrap()
                .pop()
                .map(Property::into_value);
        }
        if n as usize + 1 != self.elems.len() {
            return None;
        }
        let slot = self.elems[n as usize];
        if slot == NO_SLOT || slot as usize + 1 != self.entries.len() {
            return None;
        }
        let p = &self.entries[slot as usize].1;
        if p.accessor() || !p.configurable() {
            return None;
        }
        self.note_structural();
        let (_, p) = self.entries.pop().unwrap();
        self.elems.pop();
        if let Some(index) = self.elems.index_mut() {
            index.remove(&index_key(n as usize));
        }
        if self.mirror_flags & MIRROR_OK != 0 {
            debug_assert_eq!(self.elems.mirror_len(), self.elems.len() + 1);
            let m = self.elems.mirror_pop();
            if m.map(f64::to_bits) == Some(MIRROR_HOLE) {
                // (Unreachable while the slot was live, but keep the accounting exact.)
                self.mirror_holes -= 1;
                if self.mirror_holes == 0 {
                    self.mirror_flags |= MIRROR_NO_HOLES;
                }
            }
        }
        Some(p.into_value())
    }

    /// Append the next dense element while *building a fresh array in order* (element index ==
    /// entry slot == dense slot): skips the canonical-index parse and, for small indices, the
    /// key-string allocation. Only valid on a Props whose entries so far are exactly the dense
    /// elements 0..len.
    pub(crate) fn push_dense(&mut self, prop: Property) {
        if let Some(packed) = self.elems.packed_mut() {
            packed.push(prop);
            return;
        }
        let slot = self.entries.len();
        let key = index_key(slot);
        if let Some(index) = self.elems.index_mut() {
            index.insert(key.clone(), slot);
        }
        self.reserve_entry();
        self.entries.push((key, prop));
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
    }
}
