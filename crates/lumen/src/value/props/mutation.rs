//! Property insertion, removal, and secondary index maintenance.
use super::shapes::{shape_fresh, shape_transition, INDEX_THRESHOLD};
use super::{Props, MIRROR_ALL_I32, MIRROR_HOLE, MIRROR_NO_HOLES, MIRROR_OK, NO_SLOT};
use crate::value::{canonical_index, Property, Value};
use std::rc::Rc;

impl Props {
    /// Whether adding `key` should create the string-key hash index. Dense array elements already
    /// have O(1) lookup through `elems`, so counting them toward the generic-map threshold creates
    /// a redundant hash table for every modest-sized array. Only named properties count toward
    /// the threshold on arrays; once an index exists we continue maintaining all of its entries.
    pub(super) fn should_build_index(&self, key: &str) -> bool {
        if !self.elem_mode.get() {
            return self.entries.len() + 1 > INDEX_THRESHOLD;
        }
        if canonical_index(key).is_some() {
            return false;
        }
        self.entries
            .iter()
            .filter(|(k, _)| canonical_index(k).is_none())
            .count()
            + 1
            > INDEX_THRESHOLD
    }

    /// Build the hash index for every current entry (crossing the small-map threshold).
    pub(super) fn build_index(&mut self) {
        let mut index = Box::<crate::fasthash::FastMap<Rc<str>, usize>>::default();
        for (j, (k, _)) in self.entries.iter().enumerate() {
            index.insert(k.clone(), j);
        }
        self.elems.set_index(Some(index));
    }

    /// Drop every property (used by the GC to break a garbage object's reference cycles).
    pub(crate) fn clear(&mut self) {
        self.note_structural();
        self.entries.clear();
        self.elems.clear();
        self.elems.mirror_clear();
        self.mirror_flags = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;
        self.mirror_holes = 0;
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        self.shape = shape_fresh();
    }

    /// Insert a key *known to be absent* (the caller shape-validated the map), landing on a
    /// *known* child shape: skips both the existence scan and the transition-table lookup that
    /// [`Props::insert`] pays. `new_shape` must be the memoized `shape_transition(shape, key)`
    /// result recorded when this (shape, key) pair was first inserted the slow way.
    pub(crate) fn append_new(&mut self, key: Rc<str>, prop: Property, new_shape: u32) {
        self.note_structural();
        let slot = self.entries.len();
        if let Some(index) = self.elems.index_mut() {
            index.insert(key.clone(), slot);
        } else if self.should_build_index(&key) {
            self.build_index();
            self.elems.index_mut().unwrap().insert(key.clone(), slot);
        }
        self.shape = new_shape;
        self.reserve_entry();
        self.entries.push((key, prop));
        self.note_inserted(slot);
    }

    /// Build the named-property prefix of a brand-new ordinary object after creation ICs proved
    /// every key absent/non-indexed and supplied the complete shape chain. Up to the small-map
    /// threshold no dense/index sidecar or special-slot memo can be required, so the whole batch
    /// is just entry appends followed by its already-known final shape.
    pub(crate) fn append_proven_plain(&mut self, key: Rc<str>, prop: Property) {
        debug_assert!(self.entries.len() < INDEX_THRESHOLD);
        debug_assert!(canonical_index(&key).is_none());
        debug_assert!(self.elems.0.is_none());
        self.reserve_entry();
        self.entries.push((key, prop));
    }

    pub(crate) fn finish_proven_plain_shape(&mut self, shape: u32) {
        debug_assert!(!self.entries.is_empty());
        debug_assert!(self.entries.len() <= INDEX_THRESHOLD);
        self.shape = shape;
    }

    pub(crate) fn insert(&mut self, key: impl Into<Rc<str>>, prop: Property) {
        let key = key.into();
        if let (Some(n), Some(packed)) = (canonical_index(&key), self.elems.packed_ref()) {
            let n = n as usize;
            if n < packed.len() {
                self.note_structural();
                self.elems.packed_mut().unwrap()[n] = prop;
                return;
            }
            if !self.has_far.get() && n <= packed.len() + 256 {
                self.note_structural();
                let packed = self.elems.packed_mut().unwrap();
                packed.resize_with(n, || Property::plain(Value::Empty));
                packed.push(prop);
                return;
            }
            self.has_far.set(true);
        }
        if let Some(i) = self.find(&key) {
            self.entries[i].1 = prop;
            if self.mirror_flags & MIRROR_OK != 0
                && key.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
            {
                match canonical_index(&key) {
                    Some(n) if (n as usize) < self.elems.mirror_len() => {
                        // Replacing an existing entry: position n already had the element.
                        self.mirror_sync(n as usize, i, false)
                    }
                    // A far/map-only index entry stays outside the mirror's range: fine.
                    Some(_) => {}
                    None => self.mirror_invalidate(), // "007"-style: not canonical, be safe
                }
            }
        } else {
            self.note_structural();
            let slot = self.entries.len();
            if let Some(index) = self.elems.index_mut() {
                index.insert(key.clone(), slot);
            } else if self.should_build_index(&key) {
                self.build_index();
                self.elems.index_mut().unwrap().insert(key.clone(), slot);
            }
            if !(self.elem_mode.get() && canonical_index(&key).is_some()) {
                self.shape = shape_transition(self.shape, &key);
            }
            if &*key == "length" {
                self.len_slot.set(slot as u32);
            } else if &*key == "prototype" {
                self.proto_slot.set(slot as u32);
            }
            self.reserve_entry();
            self.entries.push((key, prop));
            self.note_inserted(slot);
        }
    }

    /// Remove every canonical-index key `>= from` in one pass — array truncation
    /// (`arr.length = n`). Entries compact and the lookup/dense maps rebuild once: O(n) total,
    /// where the per-key [`Props::remove`] loop it replaces was O(n) *per key*.
    pub(crate) fn remove_indices_from(&mut self, from: usize) {
        let keep = |k: &str| match canonical_index(k) {
            Some(n) => (n as usize) < from,
            None => true,
        };
        let packed_remove = self.elems.packed_ref().is_some_and(|p| {
            p.len() > from && p[from..].iter().any(|p| !matches!(p.value(), Value::Empty))
        });
        if !packed_remove && self.entries.iter().all(|(k, _)| keep(k)) {
            return;
        }
        self.note_structural();
        if let Some(packed) = self.elems.packed_mut() {
            packed.truncate(from);
        }
        self.entries.retain(|(k, _)| keep(k));
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        self.elems.set_index(None);
        if self.entries.len() > INDEX_THRESHOLD {
            self.build_index();
        }
        self.elems.clear_elems();
        self.mirror_flags = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;
        self.mirror_holes = 0;
        for slot in 0..self.entries.len() {
            self.note_inserted(slot);
        }
        // A removal shifts slots: it can't be a tree transition, so deopt to a fresh unique id.
        self.shape = shape_fresh();
    }

    pub(crate) fn remove(&mut self, key: &str) -> bool {
        if let (Some(n), Some(packed)) = (canonical_index(key), self.elems.packed_ref()) {
            if packed
                .get(n as usize)
                .is_some_and(|p| !matches!(p.value(), Value::Empty))
            {
                self.note_structural();
                self.elems.packed_mut().unwrap()[n as usize] = Property::plain(Value::Empty);
                return true;
            }
        }
        let Some(i) = self.find(key) else {
            return false;
        };
        self.note_structural();
        self.entries.remove(i);
        // Slots shifted — deopt to a fresh shape id (see remove_indices_from). Array maps skip
        // this for ELEMENT keys: their shape tracks named keys only, and array entry slots are
        // only ever trusted through key-checked ICs (IC_ARR_KEYCHK), which re-verify on hit.
        if !(self.elem_mode.get() && canonical_index(key).is_some()) {
            self.shape = shape_fresh();
        }
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        if let Some(index) = self.elems.index_mut() {
            index.remove(key);
            // Re-index everything after the removed slot.
            for (j, (k, _)) in self.entries.iter().enumerate().skip(i) {
                index.insert(k.clone(), j);
            }
        }
        if self.mirror_flags & MIRROR_OK != 0 {
            match canonical_index(key) {
                Some(n) if (n as usize) < self.elems.mirror_len() => {
                    if self.elems.mirror_get(n as usize).unwrap().to_bits() != MIRROR_HOLE {
                        *self.elems.mirror_get_mut(n as usize).unwrap() =
                            f64::from_bits(MIRROR_HOLE);
                        self.mirror_flags &= !MIRROR_NO_HOLES;
                        self.mirror_holes += 1;
                    }
                }
                Some(_) | None => {}
            }
        }
        // Dense slots shift down past the removed entry; the removed key's own slot holes.
        for e in self.elems.iter_mut() {
            if *e == NO_SLOT {
                continue;
            }
            match (*e as usize).cmp(&i) {
                std::cmp::Ordering::Equal => *e = NO_SLOT,
                std::cmp::Ordering::Greater => *e -= 1,
                std::cmp::Ordering::Less => {}
            }
        }
        true
    }
}
