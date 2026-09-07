//! Optional dense buffers and inline packed property ownership.
use crate::value::{Property, Value};
use std::rc::Rc;
/// Insertion-ordered string-keyed property map. A `Vec` of entries preserves order (good enough for
/// `for-in`/`Object.keys`); a side `HashMap` keeps lookup O(1).
pub(super) const INLINE_PACKED_CAPACITY: usize = 10;

pub(super) struct InlinePacked {
    pub(in crate::value) len: u8,
    pub(in crate::value) slots: [std::mem::MaybeUninit<Property>; INLINE_PACKED_CAPACITY],
}

impl InlinePacked {
    pub(in crate::value) const EMPTY: InlinePacked = InlinePacked {
        len: 0,
        slots: [const { std::mem::MaybeUninit::uninit() }; INLINE_PACKED_CAPACITY],
    };

    pub(in crate::value) unsafe fn from_raw(items: *mut Value, len: usize) -> InlinePacked {
        debug_assert!(len <= INLINE_PACKED_CAPACITY);
        let mut packed = InlinePacked::default();
        for index in 0..len {
            packed.slots[index].write(Property::plain(unsafe { items.add(index).read() }));
        }
        packed.len = len as u8;
        packed
    }

    pub(in crate::value) fn as_slice(&self) -> &[Property] {
        unsafe {
            std::slice::from_raw_parts(self.slots.as_ptr().cast::<Property>(), self.len as usize)
        }
    }

    pub(in crate::value) fn into_vec(&mut self) -> Vec<Property> {
        let len = self.len as usize;
        let mut values = Vec::with_capacity(len);
        for index in 0..len {
            values.push(unsafe { self.slots[index].assume_init_read() });
        }
        self.len = 0;
        values
    }
}

impl Default for InlinePacked {
    fn default() -> Self {
        InlinePacked::EMPTY
    }
}

impl Clone for InlinePacked {
    fn clone(&self) -> Self {
        let mut clone = InlinePacked::default();
        for (index, property) in self.as_slice().iter().enumerate() {
            clone.slots[index].write(property.clone());
        }
        clone.len = self.len;
        clone
    }
}

impl Drop for InlinePacked {
    fn drop(&mut self) {
        for index in 0..self.len as usize {
            unsafe { self.slots[index].assume_init_drop() };
        }
    }
}

#[derive(Clone, Default)]
pub(in crate::value) struct DenseBuffers {
    pub(in crate::value) index: Option<Box<crate::fasthash::FastMap<Rc<str>, usize>>>,
    pub(in crate::value) packed: Option<Box<Vec<Property>>>,
    pub(super) inline_packed: InlinePacked,
    pub(in crate::value) elems: Vec<u32>,
    pub(in crate::value) mirror: Vec<f64>,
}

struct EmptyDenseBuffers(DenseBuffers);
// This one value contains only `None` and empty Vec dangling sentinels and is never mutated; no
// non-Sync payload is reachable through it. Live DenseBuffers remain thread-local as before.
unsafe impl Sync for EmptyDenseBuffers {}

static EMPTY_DENSE_BUFFERS: EmptyDenseBuffers = EmptyDenseBuffers(DenseBuffers {
    index: None,
    packed: None,
    inline_packed: InlinePacked::EMPTY,
    elems: Vec::new(),
    mirror: Vec::new(),
});

#[derive(Clone, Default)]
#[repr(transparent)]
pub(in crate::value) struct DenseStorage(pub(in crate::value) Option<Box<DenseBuffers>>);

impl std::ops::Deref for DenseStorage {
    type Target = DenseBuffers;
    fn deref(&self) -> &DenseBuffers {
        self.0.as_deref().unwrap_or(&EMPTY_DENSE_BUFFERS.0)
    }
}

impl DenseStorage {
    #[inline]
    pub(in crate::value) fn buffers_mut(&mut self) -> &mut DenseBuffers {
        self.0.get_or_insert_with(Default::default)
    }
    pub(in crate::value) fn index_mut(
        &mut self,
    ) -> Option<&mut crate::fasthash::FastMap<Rc<str>, usize>> {
        self.0.as_deref_mut()?.index.as_deref_mut()
    }
    pub(in crate::value) fn packed_mut(&mut self) -> Option<&mut Vec<Property>> {
        let dense = self.0.as_deref_mut()?;
        if dense.packed.is_none() && dense.inline_packed.len != 0 {
            dense.packed = Some(Box::new(dense.inline_packed.into_vec()));
        }
        dense.packed.as_deref_mut()
    }
    pub(in crate::value) fn packed_ref(&self) -> Option<&[Property]> {
        let dense = self.0.as_deref()?;
        match dense.packed.as_deref() {
            Some(packed) => Some(packed),
            None if dense.inline_packed.len != 0 => Some(dense.inline_packed.as_slice()),
            None => None,
        }
    }
    pub(in crate::value) fn packed_is_some(&self) -> bool {
        self.packed_ref().is_some()
    }
    pub(in crate::value) fn set_index(
        &mut self,
        index: Option<Box<crate::fasthash::FastMap<Rc<str>, usize>>>,
    ) {
        if index.is_some() {
            self.buffers_mut().index = index;
        } else if let Some(d) = self.0.as_deref_mut() {
            d.index = None;
        }
    }
    pub(in crate::value) fn set_packed(&mut self, packed: Option<Box<Vec<Property>>>) {
        if packed.is_some() {
            let dense = self.buffers_mut();
            dense.inline_packed = InlinePacked::default();
            dense.packed = packed;
        } else if let Some(d) = self.0.as_deref_mut() {
            d.packed = None;
            d.inline_packed = InlinePacked::default();
        }
    }
    #[inline]
    pub(in crate::value) fn len(&self) -> usize {
        self.0.as_deref().map_or(0, |d| d.elems.len())
    }
    #[inline]
    pub(in crate::value) fn get(&self, index: usize) -> Option<&u32> {
        self.0.as_deref().and_then(|d| d.elems.get(index))
    }
    #[inline]
    pub(in crate::value) fn get_mut(&mut self, index: usize) -> Option<&mut u32> {
        self.0.as_deref_mut().and_then(|d| d.elems.get_mut(index))
    }
    pub(in crate::value) fn reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.buffers_mut().elems.reserve_exact(additional);
        }
    }
    #[inline]
    pub(in crate::value) fn push(&mut self, value: u32) {
        self.buffers_mut().elems.push(value);
    }
    #[inline]
    pub(in crate::value) fn pop(&mut self) -> Option<u32> {
        self.0.as_deref_mut().and_then(|d| d.elems.pop())
    }
    pub(in crate::value) fn clear(&mut self) {
        self.0 = None;
    }
    pub(in crate::value) fn clear_elems(&mut self) {
        if let Some(d) = self.0.as_deref_mut() {
            d.elems.clear();
            d.mirror.clear();
        }
    }
    pub(in crate::value) fn iter_mut(&mut self) -> std::slice::IterMut<'_, u32> {
        self.buffers_mut().elems.iter_mut()
    }

    pub(in crate::value) fn mirror_reserve_exact(&mut self, additional: usize) {
        if additional != 0 {
            self.buffers_mut().mirror.reserve_exact(additional);
        }
    }
    pub(in crate::value) fn mirror_len(&self) -> usize {
        self.0.as_deref().map_or(0, |d| d.mirror.len())
    }
    pub(in crate::value) fn mirror_get(&self, index: usize) -> Option<&f64> {
        self.0.as_deref().and_then(|d| d.mirror.get(index))
    }
    pub(in crate::value) fn mirror_get_mut(&mut self, index: usize) -> Option<&mut f64> {
        self.0.as_deref_mut().and_then(|d| d.mirror.get_mut(index))
    }
    pub(in crate::value) fn mirror_push(&mut self, value: f64) {
        self.buffers_mut().mirror.push(value);
    }
    pub(in crate::value) fn mirror_pop(&mut self) -> Option<f64> {
        self.0.as_deref_mut().and_then(|d| d.mirror.pop())
    }
    pub(in crate::value) fn mirror_clear(&mut self) {
        if let Some(d) = self.0.as_deref_mut() {
            d.mirror.clear();
        }
    }
    pub(in crate::value) fn mirror_extend<I: IntoIterator<Item = f64>>(&mut self, iter: I) {
        self.buffers_mut().mirror.extend(iter);
    }
}

impl std::ops::Index<usize> for DenseStorage {
    type Output = u32;
    fn index(&self, index: usize) -> &u32 {
        self.get(index).expect("dense index out of bounds")
    }
}

impl std::ops::IndexMut<usize> for DenseStorage {
    fn index_mut(&mut self, index: usize) -> &mut u32 {
        self.get_mut(index).expect("dense index out of bounds")
    }
}
