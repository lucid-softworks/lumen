//! Named lookup, slot access, and ordered reflection.
use super::shapes::index_key;
use super::{Props, NO_SLOT};
use crate::value::{canonical_index, Property, Value};
use std::rc::Rc;

impl Props {
    /// The entry slot for `key`. Small maps (≤ [`super::shapes::INDEX_THRESHOLD`] entries — most objects) have
    /// no hash index at all: lookup is a short linear scan and inserts never hash or rehash.
    /// The index is built once when a map grows past the threshold and is authoritative from
    /// then on (an emptied-but-once-large map keeps using it).
    #[inline(always)]
    pub(super) fn find(&self, key: &str) -> Option<usize> {
        // `length` and `prototype` are the hottest keys in array-heavy / allocation-heavy code
        // (every push/pop/length read; every `new`); their slots are memoized — answer without
        // hashing or scanning.
        if key == "length" {
            let s = self.len_slot.get();
            if s != NO_SLOT {
                debug_assert!(
                    matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "length")
                );
                return Some(s as usize);
            }
        } else if key == "prototype" {
            let s = self.proto_slot.get();
            if s != NO_SLOT {
                debug_assert!(
                    matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "prototype")
                );
                return Some(s as usize);
            }
        }
        let found = if self.elems.index.is_none() {
            self.entries.iter().position(|(k, _)| &**k == key)
        } else {
            self.elems
                .index
                .as_ref()
                .and_then(|index| index.get(key).copied())
        };
        if let Some(s) = found {
            if key == "length" {
                self.len_slot.set(s as u32);
            } else if key == "prototype" {
                self.proto_slot.set(s as u32);
            }
        }
        found
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Property> {
        if let Some(n) = canonical_index(key) {
            if let Some(p) = self.get_index(n) {
                return Some(p);
            }
            // Every canonical index is recorded in the dense sidecar unless a deliberately
            // sparse, far-ahead insertion has ever occurred. With no such insertion, a dense
            // miss proves absence; scanning every string-key entry is both redundant and
            // especially costly for a hole read from a large array.
            if !self.has_far.get() {
                return None;
            }
        }
        self.find(key).map(|i| &self.entries[i].1)
    }

    /// The memoized own `prototype` slot, for guarded constructor fast paths.
    #[inline]
    pub(crate) fn prototype_slot(&self) -> Option<u32> {
        self.find("prototype").map(|slot| slot as u32)
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut Property> {
        if let Some(n) = canonical_index(key) {
            if self
                .elems
                .packed_ref()
                .and_then(|p| p.get(n as usize))
                .is_some_and(|p| !matches!(p.value(), Value::Empty))
            {
                return self.elems.packed_mut().and_then(|p| p.get_mut(n as usize));
            }
        }
        if key.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        match self.find(key) {
            Some(i) => Some(&mut self.entries[i].1),
            None => None,
        }
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The `entries` slot for `key`, or `None`. Backs the bytecode property inline cache: a hit
    /// records the slot so the next access can skip the lookup (see `Interp::try_ic_get`).
    #[inline]
    pub(crate) fn slot_of(&self, key: &str) -> Option<usize> {
        self.find(key)
    }

    /// The (key, property) at `slot`, or `None` if out of range. The caller re-checks the key —
    /// slots shift on `remove`, so a cached slot is only trusted after the key matches.
    #[inline]
    pub(crate) fn entry_at(&self, slot: usize) -> Option<&(Rc<str>, Property)> {
        self.entries.get(slot)
    }

    /// Mutable [`entry_at`], for the property write inline cache.
    #[inline]
    pub(crate) fn entry_at_mut(&mut self, slot: usize) -> Option<&mut (Rc<str>, Property)> {
        if self
            .entries
            .get(slot)
            .is_some_and(|(k, _)| k.as_bytes().first().is_some_and(|b| b.is_ascii_digit()))
        {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        self.entries.get_mut(slot)
    }

    /// Keys in insertion order. Private-name slots (`#x`) are never enumerable/observable, so they
    /// are excluded here (and from [`ordered_keys`]); private access reads them via [`get`] directly.
    pub(crate) fn keys(&self) -> Vec<Rc<str>> {
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter().enumerate())
            .filter(|(_, p)| !matches!(p.value(), Value::Empty))
            .map(|(n, _)| index_key(n))
            .chain(self.entries.iter().map(|(k, _)| k.clone()))
            .filter(|k| !crate::interpreter::Interp::is_private_key(k))
            .collect()
    }

    /// Keys in spec [[OwnPropertyKeys]] order: array-index keys ascending, then other string keys
    /// in insertion order, then symbol keys in insertion order.
    pub(crate) fn ordered_keys(&self) -> Vec<Rc<str>> {
        let mut ints: Vec<(u32, Rc<str>)> = Vec::new();
        let mut strs: Vec<Rc<str>> = Vec::new();
        let mut syms: Vec<Rc<str>> = Vec::new();
        if let Some(packed) = self.elems.packed_ref() {
            ints.extend(
                packed
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !matches!(p.value(), Value::Empty))
                    .map(|(n, _)| (n as u32, index_key(n))),
            );
        }
        for (k, _) in &self.entries {
            if crate::interpreter::Interp::is_private_key(k) {
                continue; // private-element slot — not an observable own key
            }
            if crate::interpreter::Interp::is_sym_key(k) {
                syms.push(k.clone());
            } else if let Some(n) = canonical_index(k) {
                ints.push((n, k.clone()));
            } else {
                strs.push(k.clone());
            }
        }
        ints.sort_by_key(|(n, _)| *n);
        ints.into_iter()
            .map(|(_, k)| k)
            .chain(strs)
            .chain(syms)
            .collect()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Property)> {
        self.entries.iter().map(|(k, p)| (k, p))
    }

    /// Every live property value, including keyless packed elements (for GC tracing).
    pub(crate) fn values(&self) -> impl Iterator<Item = &Property> {
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter())
            .filter(|p| !matches!(p.value(), Value::Empty))
            .chain(self.entries.iter().map(|(_, p)| p))
    }

    pub(crate) fn highest_nonconfig_index_from(&self, from: usize) -> Option<usize> {
        let packed = self
            .elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter().enumerate())
            .filter_map(|(n, p)| {
                (!matches!(p.value(), Value::Empty) && !p.configurable() && n >= from).then_some(n)
            });
        let entries = self.entries.iter().filter_map(|(k, p)| {
            (!p.configurable())
                .then(|| canonical_index(k).map(|n| n as usize))
                .flatten()
                .filter(|&n| n >= from)
        });
        packed.chain(entries).max()
    }

    pub(crate) fn integrity_ok(&self, frozen: bool) -> bool {
        let valid = |p: &Property| !p.configurable() && (!frozen || p.accessor() || !p.writable());
        self.elems
            .packed_ref()
            .into_iter()
            .flat_map(|p| p.iter())
            .filter(|p| !matches!(p.value(), Value::Empty))
            .all(valid)
            && self
                .entries
                .iter()
                .all(|(k, p)| crate::interpreter::Interp::is_private_key(k) || valid(p))
    }
}
