//! Binding maps with generation-checked entry addresses.
use super::Binding;
use crate::value::Value;
use std::rc::Rc;

mod layout;
pub(crate) use layout::BindingLayout;

/// One scope's binding map, wrapping the raw hash map so every *structural* mutation — anything
/// that can move entries or change what a name resolves to (insert, remove, clear) — bumps a
/// generation counter. The bytecode tier's per-site name caches hold a raw `&Binding` pointer
/// plus the generation they resolved it at (see `bytecode::NameIc`): a matching generation
/// proves the map hasn't changed shape since, so the pointer is still valid *and* still the
/// right resolution. In-place binding writes (`get_mut`) intentionally don't bump — they can't
/// move entries, and a cache read-through observes the new value, which is exactly correct.
/// Reads pass through via `Deref`; mutations only exist as the inherent methods below, so a new
/// mutation site can't forget the bump (it won't compile).
pub struct VarMap {
    map: VarStorage,
    generation: std::cell::Cell<u32>,
}

const SMALL_VAR_MAP_CAPACITY: usize = 8;

enum VarStorage {
    Template(Rc<BindingLayout>, Vec<Binding>),
    Small(Vec<(std::rc::Rc<str>, Binding)>),
    Large(crate::fasthash::FastMap<std::rc::Rc<str>, Binding>),
}

impl Default for VarMap {
    fn default() -> Self {
        Self {
            map: VarStorage::Small(Vec::new()),
            generation: std::cell::Cell::new(0),
        }
    }
}

pub enum VarIter<'a> {
    Template(std::iter::Zip<std::slice::Iter<'a, Rc<str>>, std::slice::Iter<'a, Binding>>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Iter<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarIter<'a> {
    type Item = (&'a std::rc::Rc<str>, &'a Binding);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarIter::Small(iter) => iter.next().map(|(name, binding)| (name, binding)),
            VarIter::Large(iter) => iter.next(),
            VarIter::Template(iter) => iter.next(),
        }
    }
}

pub enum VarKeys<'a> {
    Template(std::slice::Iter<'a, Rc<str>>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Keys<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarKeys<'a> {
    type Item = &'a std::rc::Rc<str>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarKeys::Small(iter) => iter.next().map(|(name, _)| name),
            VarKeys::Large(iter) => iter.next(),
            VarKeys::Template(iter) => iter.next(),
        }
    }
}

pub enum VarValues<'a> {
    Template(std::slice::Iter<'a, Binding>),
    Small(std::slice::Iter<'a, (std::rc::Rc<str>, Binding)>),
    Large(std::collections::hash_map::Values<'a, std::rc::Rc<str>, Binding>),
}

impl<'a> Iterator for VarValues<'a> {
    type Item = &'a Binding;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            VarValues::Small(iter) => iter.next().map(|(_, binding)| binding),
            VarValues::Large(iter) => iter.next(),
            VarValues::Template(iter) => iter.next(),
        }
    }
}

impl VarMap {
    pub(crate) fn template_layout(&self) -> Option<&Rc<BindingLayout>> {
        match &self.map {
            VarStorage::Template(layout, _) if self.generation() == 0 => Some(layout),
            _ => None,
        }
    }

    pub(crate) fn template_binding(&self, slot: usize) -> Option<&Binding> {
        match &self.map {
            VarStorage::Template(_, values) if self.generation() == 0 => values.get(slot),
            _ => None,
        }
    }

    pub(crate) fn layout_base(&mut self, expected: &Rc<BindingLayout>) -> Option<*mut Binding> {
        match &mut self.map {
            VarStorage::Template(layout, values) if Rc::ptr_eq(layout, expected) => {
                Some(values.as_mut_ptr())
            }
            _ => None,
        }
    }

    pub(crate) fn from_layout(layout: Rc<BindingLayout>) -> Self {
        let values = layout
            .names
            .iter()
            .map(|_| Binding::data(Value::Undefined, true, true))
            .collect();
        Self {
            map: VarStorage::Template(layout, values),
            generation: std::cell::Cell::new(0),
        }
    }

    /// The layout identity proves the slot's name without a string/hash lookup. In-place
    /// writes keep entry addresses stable, just like get_mut on the dynamic representation.
    pub(crate) fn layout_binding_mut(
        &mut self,
        expected: &Rc<BindingLayout>,
        slot: usize,
    ) -> Option<&mut Binding> {
        match &mut self.map {
            VarStorage::Template(layout, values) if Rc::ptr_eq(layout, expected) => {
                values.get_mut(slot)
            }
            _ => None,
        }
    }

    fn make_dynamic(&mut self) {
        if !matches!(self.map, VarStorage::Template(..)) {
            return;
        }
        let VarStorage::Template(layout, values) =
            std::mem::replace(&mut self.map, VarStorage::Small(Vec::new()))
        else {
            unreachable!()
        };
        self.map = VarStorage::Large(layout.names.iter().cloned().zip(values).collect());
    }

    pub(crate) fn with_capacity(capacity: usize) -> VarMap {
        VarMap {
            map: if capacity <= SMALL_VAR_MAP_CAPACITY {
                VarStorage::Small(Vec::with_capacity(capacity))
            } else {
                VarStorage::Large(crate::fasthash::FastMap::with_capacity_and_hasher(
                    capacity,
                    Default::default(),
                ))
            },
            generation: std::cell::Cell::new(0),
        }
    }

    /// The structural generation (name-cache validation token).
    #[inline]
    pub(crate) fn generation(&self) -> u32 {
        self.generation.get()
    }
    #[inline]
    fn bump(&self) {
        // Zero is reserved for pristine compiled layouts. Once structural mutation has
        // invalidated an activation's native binding base, wrapping must never revive it.
        self.generation
            .set(self.generation.get().wrapping_add(1).max(1));
    }
    pub fn insert(&mut self, k: impl Into<std::rc::Rc<str>>, v: Binding) -> Option<Binding> {
        self.bump();
        let k = k.into();
        if matches!(&self.map, VarStorage::Template(layout, _) if layout.slot(&k).is_none()) {
            self.make_dynamic();
        }
        match &mut self.map {
            VarStorage::Template(layout, values) => {
                let slot = layout.slot(&k).expect("existing layout binding");
                Some(std::mem::replace(&mut values[slot], v))
            }
            VarStorage::Small(entries) => {
                if let Some((_, old)) = entries.iter_mut().find(|(name, _)| **name == *k) {
                    return Some(std::mem::replace(old, v));
                }
                if entries.len() < SMALL_VAR_MAP_CAPACITY {
                    entries.push((k, v));
                    return None;
                }
                let mut large = crate::fasthash::FastMap::with_capacity_and_hasher(
                    entries.len() + 1,
                    Default::default(),
                );
                for (name, binding) in std::mem::take(entries) {
                    large.insert(name, binding);
                }
                let old = large.insert(k, v);
                self.map = VarStorage::Large(large);
                old
            }
            VarStorage::Large(entries) => entries.insert(k, v),
        }
    }
    pub fn remove(&mut self, k: &str) -> Option<Binding> {
        self.bump();
        self.make_dynamic();
        match &mut self.map {
            VarStorage::Small(entries) => entries
                .iter()
                .position(|(name, _)| &**name == k)
                .map(|index| entries.swap_remove(index).1),
            VarStorage::Large(entries) => entries.remove(k),
            VarStorage::Template(..) => unreachable!("promoted above"),
        }
    }
    pub fn clear(&mut self) {
        self.bump();
        match &mut self.map {
            VarStorage::Small(entries) => entries.clear(),
            VarStorage::Large(entries) => entries.clear(),
            VarStorage::Template(..) => self.map = VarStorage::Small(Vec::new()),
        }
    }
    /// In-place binding write: entries don't move, so the generation stays (see the type docs).
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Binding> {
        match &mut self.map {
            VarStorage::Small(entries) => entries
                .iter_mut()
                .find(|(name, _)| &**name == k)
                .map(|(_, binding)| binding),
            VarStorage::Large(entries) => entries.get_mut(k),
            VarStorage::Template(layout, values) => layout.slot(k).map(|slot| &mut values[slot]),
        }
    }
    pub fn get(&self, k: &str) -> Option<&Binding> {
        match &self.map {
            VarStorage::Small(entries) => entries
                .iter()
                .find(|(name, _)| &**name == k)
                .map(|(_, binding)| binding),
            VarStorage::Large(entries) => entries.get(k),
            VarStorage::Template(layout, values) => layout.slot(k).map(|slot| &values[slot]),
        }
    }
    pub fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    pub fn iter(&self) -> VarIter<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarIter::Small(entries.iter()),
            VarStorage::Large(entries) => VarIter::Large(entries.iter()),
            VarStorage::Template(layout, values) => {
                VarIter::Template(layout.names.iter().zip(values.iter()))
            }
        }
    }
    pub fn keys(&self) -> VarKeys<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarKeys::Small(entries.iter()),
            VarStorage::Large(entries) => VarKeys::Large(entries.keys()),
            VarStorage::Template(layout, _) => VarKeys::Template(layout.names.iter()),
        }
    }
    pub fn values(&self) -> VarValues<'_> {
        match &self.map {
            VarStorage::Small(entries) => VarValues::Small(entries.iter()),
            VarStorage::Large(entries) => VarValues::Large(entries.values()),
            VarStorage::Template(_, values) => VarValues::Template(values.iter()),
        }
    }
    /// Byte offset of the generation counter within a `VarMap` (for the JIT's inline template).
    pub(crate) fn generation_offset() -> usize {
        std::mem::offset_of!(VarMap, generation)
    }
}
