//! Insertion-ordered collection storage. The index owns only slot numbers, never JS values.
//! Deleted slots remain in place so live iterators and forEach can observe later appends.

use super::same_value_zero;
use crate::fasthash::{FastMap, FxHasher};
use crate::value::Value;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

const NO_SLOT: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    #[default]
    Map,
    Set,
    WeakMap,
    WeakSet,
}

impl CollectionKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Map => "Map",
            Self::Set => "Set",
            Self::WeakMap => "WeakMap",
            Self::WeakSet => "WeakSet",
        }
    }
}

struct Entry {
    pair: Option<(Value, Value)>,
    // Intrusive collision chain avoids allocating a Vec for every distinct key hash.
    next: usize,
}

#[derive(Default)]
pub(crate) struct CollectionData {
    kind: CollectionKind,
    entries: Vec<Entry>,
    // Logical position of entries[0], retained when clear releases the backing list.
    base: usize,
    // Hash collisions are resolved with SameValueZero, not hash equality alone.
    index: FastMap<u64, usize>,
    live_len: usize,
}

impl CollectionData {
    pub(crate) fn new(kind: CollectionKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    pub(crate) fn kind(&self) -> CollectionKind {
        self.kind
    }

    pub(crate) fn len(&self) -> usize {
        self.live_len
    }

    pub(crate) fn next(&self, cursor: &mut usize) -> Option<&(Value, Value)> {
        *cursor = (*cursor).max(self.base);
        while let Some(entry) = self.entries.get(*cursor - self.base) {
            *cursor += 1;
            if let Some(pair) = &entry.pair {
                return Some(pair);
            }
        }
        None
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().filter_map(|entry| entry.pair.as_ref())
    }

    fn find(&self, key: &Value, hash: u64) -> Option<usize> {
        let mut slot = *self.index.get(&hash)?;
        while slot != NO_SLOT {
            let entry = &self.entries[slot];
            if same_value_zero(&entry.pair.as_ref().unwrap().0, key) {
                return Some(slot);
            }
            slot = entry.next;
        }
        None
    }

    pub(crate) fn lookup(&self, key: &Value) -> Option<&Value> {
        self.find(key, key_hash(key))
            .map(|slot| &self.entries[slot].pair.as_ref().unwrap().1)
    }

    pub(crate) fn contains(&self, key: &Value) -> bool {
        self.lookup(key).is_some()
    }

    pub(crate) fn insert(&mut self, key: Value, value: Value) {
        let key = match key {
            Value::Num(n) if n == 0.0 && n.is_sign_negative() => Value::Num(0.0),
            other => other,
        };
        let hash = key_hash(&key);
        let next = match self.index.entry(hash) {
            std::collections::hash_map::Entry::Occupied(mut head) => {
                let mut slot = *head.get();
                while slot != NO_SLOT {
                    let entry = &mut self.entries[slot];
                    let pair = entry.pair.as_mut().unwrap();
                    if same_value_zero(&pair.0, &key) {
                        pair.1 = value;
                        return;
                    }
                    slot = entry.next;
                }
                head.insert(self.entries.len())
            }
            std::collections::hash_map::Entry::Vacant(head) => {
                head.insert(self.entries.len());
                NO_SLOT
            }
        };
        self.entries.push(Entry {
            pair: Some((key, value)),
            next,
        });
        self.live_len += 1;
    }

    pub(crate) fn remove(&mut self, key: &Value) -> bool {
        let hash = key_hash(key);
        let Some(&head) = self.index.get(&hash) else {
            return false;
        };
        let mut slot = head;
        let mut previous = NO_SLOT;
        while slot != NO_SLOT {
            let entry = &self.entries[slot];
            let next = entry.next;
            if same_value_zero(&entry.pair.as_ref().unwrap().0, key) {
                if previous != NO_SLOT {
                    self.entries[previous].next = next;
                } else if next == NO_SLOT {
                    self.index.remove(&hash);
                } else {
                    self.index.insert(hash, next);
                }
                self.entries[slot].pair = None;
                self.live_len -= 1;
                return true;
            }
            previous = slot;
            slot = next;
        }
        false
    }

    /// Weak collections have no live iterators, so periodically discard their vacant slots.
    pub(crate) fn remove_weak(&mut self, key: &Value) -> bool {
        let removed = self.remove(key);
        if removed && self.entries.len() > self.live_len.saturating_mul(2) {
            self.entries.retain(|entry| entry.pair.is_some());
            self.index.clear();
            for (slot, entry) in self.entries.iter_mut().enumerate() {
                let hash = key_hash(&entry.pair.as_ref().unwrap().0);
                entry.next = self.index.insert(hash, slot).unwrap_or(NO_SLOT);
            }
        }
        removed
    }

    pub(crate) fn clear(&mut self) {
        // Older cursors jump to this base, so clear can release every key/value and slot.
        self.base += self.entries.len();
        self.entries.clear();
        self.index.clear();
        self.live_len = 0;
    }
}

impl FromIterator<(Value, Value)> for CollectionData {
    fn from_iter<T: IntoIterator<Item = (Value, Value)>>(iter: T) -> Self {
        let mut data = Self::default();
        for (key, value) in iter {
            data.insert(key, value);
        }
        data
    }
}

fn key_hash(key: &Value) -> u64 {
    let mut hash = FxHasher::default();
    std::mem::discriminant(key).hash(&mut hash);
    match key {
        Value::Num(n) => {
            let bits = if n.is_nan() {
                f64::NAN.to_bits()
            } else if *n == 0.0 {
                0
            } else {
                n.to_bits()
            };
            bits.hash(&mut hash);
        }
        Value::Bool(v) => v.hash(&mut hash),
        Value::BigInt(v) => v.hash(&mut hash),
        Value::Str(v) => (**v).hash(&mut hash),
        Value::Sym(v) => Rc::as_ptr(v).hash(&mut hash),
        Value::Obj(v) => Rc::as_ptr(v).hash(&mut hash),
        Value::Undefined | Value::Empty | Value::Null => {}
    }
    // FxHash preserves common low bits in integer-valued doubles. Avalanche before using
    // this fingerprint as a FastMap key, otherwise even distinct hashes cluster quadratically.
    let mut bits = hash.finish();
    bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    bits ^ (bits >> 31)
}

#[cfg(test)]
mod tests {
    use super::{key_hash, CollectionData};
    use crate::fasthash::FxHasher;
    use crate::value::{Object, Value};
    use std::hash::{Hash, Hasher};
    use std::rc::Rc;

    #[test]
    fn colliding_keys_remain_distinct_through_updates_and_deletes() {
        let text = Value::str("collision");
        // Invert FxHasher's final multiply/xor step to create a real Number/String collision.
        let mut prefix = FxHasher::default();
        std::mem::discriminant(&Value::Num(1.0)).hash(&mut prefix);
        let mut text_hash = FxHasher::default();
        std::mem::discriminant(&text).hash(&mut text_hash);
        "collision".hash(&mut text_hash);
        let bits =
            text_hash.finish().wrapping_mul(0x2040_003d_7809_70bd) ^ prefix.finish().rotate_left(5);
        let number = Value::Num(f64::from_bits(bits));
        assert_eq!(key_hash(&number), key_hash(&text));
        let mut data = CollectionData::default();
        data.insert(text.clone(), Value::Num(1.0));
        data.insert(number.clone(), Value::Num(2.0));
        data.insert(text.clone(), Value::Num(3.0));
        assert_eq!(data.len(), 2);
        assert!(matches!(data.lookup(&text), Some(Value::Num(3.0))));
        assert!(matches!(data.lookup(&number), Some(Value::Num(2.0))));
        assert!(data.remove(&text));
        assert!(!data.contains(&text));
        assert!(data.contains(&number));
        data.insert(text.clone(), Value::Num(4.0));
        assert!(data.remove(&number));
        assert!(matches!(data.lookup(&text), Some(Value::Num(4.0))));
    }

    #[test]
    fn nan_payloads_and_signed_zero_share_entries() {
        let mut data = CollectionData::default();
        data.insert(Value::Num(-0.0), Value::Num(1.0));
        data.insert(Value::Num(0.0), Value::Num(2.0));
        data.insert(Value::Num(f64::NAN), Value::Num(3.0));
        let other_nan = Value::Num(f64::from_bits(0xfff8_0000_0000_0042));
        data.insert(other_nan.clone(), Value::Num(4.0));
        assert_eq!(data.len(), 2);
        assert!(matches!(data.iter().next(), Some((Value::Num(n), _)) if n.to_bits() == 0));
        assert!(matches!(
            data.lookup(&Value::Num(f64::NAN)),
            Some(Value::Num(4.0))
        ));
        assert!(data.remove(&other_nan));
        assert!(data.remove(&Value::Num(-0.0)));
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn clear_releases_entries_without_invalidating_cursors() {
        let mut data = CollectionData::default();
        let key = Object::new(None);
        let value = Object::new(None);
        data.insert(Value::Obj(key.clone()), Value::Obj(value.clone()));
        let mut cursor = 0;
        assert!(data.next(&mut cursor).is_some());
        data.clear();
        assert_eq!(Rc::strong_count(&key), 1);
        assert_eq!(Rc::strong_count(&value), 1);
        assert!(data.entries.is_empty());
        data.insert(Value::Num(42.0), Value::Undefined);
        assert!(matches!(
            data.next(&mut cursor),
            Some((Value::Num(42.0), _))
        ));
        // An iterator created before clear but never started sees the new entry too.
        assert!(matches!(data.next(&mut 0), Some((Value::Num(42.0), _))));
    }

    #[test]
    fn delete_releases_references_and_reinsertion_appends() {
        let mut data = CollectionData::default();
        let key = Object::new(None);
        let value = Object::new(None);
        data.insert(Value::Obj(key.clone()), Value::Obj(value.clone()));
        data.insert(Value::Num(1.0), Value::Undefined);
        assert!(data.remove(&Value::Obj(key.clone())));
        assert_eq!(Rc::strong_count(&key), 1);
        assert_eq!(Rc::strong_count(&value), 1);
        data.insert(Value::Obj(key.clone()), Value::Undefined);
        let mut cursor = 0;
        assert!(matches!(data.next(&mut cursor), Some((Value::Num(1.0), _))));
        assert!(matches!(data.next(&mut cursor), Some((Value::Obj(o), _)) if Rc::ptr_eq(o, &key)));
        assert!(data.next(&mut cursor).is_none());
    }
    #[test]
    fn weak_deletion_churn_does_not_accumulate_vacant_slots() {
        let mut data = CollectionData::default();
        let stable = Object::new(None);
        data.insert(Value::Obj(stable.clone()), Value::Num(42.0));
        for _ in 0..1000 {
            let temporary = Value::Obj(Object::new(None));
            data.insert(temporary.clone(), Value::Undefined);
            assert!(data.remove_weak(&temporary));
            assert!(!data.contains(&temporary));
        }
        assert!(data.entries.len() <= 2);
        assert!(matches!(
            data.lookup(&Value::Obj(stable)),
            Some(Value::Num(42.0))
        ));
    }
}
