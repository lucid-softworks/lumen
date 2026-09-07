//! Bounded direct addressing for nearby nonnegative integer keys.
//! Slots point into the same insertion-ordered entry list as the hash index.
use crate::value::Value;

const LIMIT: usize = 65536;
const MAX_GAP: usize = 64;
const EMPTY: u32 = u32::MAX;

// Keep absent indexes to one pointer in string-keyed and weak collections.
#[allow(clippy::box_collection)]
#[derive(Default)]
pub(super) struct DenseIndex(Option<Box<Vec<u32>>>);

fn integer_key(key: &Value) -> Option<usize> {
    let Value::Num(number) = key else { return None };
    let index = *number as usize;
    (index < LIMIT && *number == index as f64).then_some(index)
}

impl DenseIndex {
    pub(super) fn candidate(&self, key: &Value, entry_slot: usize) -> Option<usize> {
        let index = integer_key(key)?;
        let len = self.0.as_ref().map_or(0, |slots| slots.len());
        (index <= len + MAX_GAP && (entry_slot < EMPTY as usize || self.get(index).is_some()))
            .then_some(index)
    }

    pub(super) fn get(&self, index: usize) -> Option<usize> {
        self.0
            .as_ref()?
            .get(index)
            .copied()
            .filter(|&slot| slot != EMPTY)
            .map(|slot| slot as usize)
    }

    pub(super) fn lookup(&self, key: &Value) -> Option<usize> {
        self.0.as_ref()?;
        self.get(integer_key(key)?)
    }

    pub(super) fn insert(&mut self, index: usize, slot: usize) {
        debug_assert!(index < LIMIT && slot < EMPTY as usize);
        let slots = self.0.get_or_insert_with(|| Box::new(Vec::new()));
        if slots.len() <= index {
            if slots.capacity() <= index {
                let capacity = slots.capacity().saturating_mul(2).max(index + 1).min(LIMIT);
                slots.reserve_exact(capacity - slots.len());
            }
            slots.resize(index, EMPTY);
            slots.push(slot as u32);
        } else {
            slots[index] = slot as u32;
        }
    }

    pub(super) fn remove(&mut self, key: &Value) -> Option<usize> {
        let index = integer_key(key)?;
        let slot = self.0.as_mut()?.get_mut(index)?;
        (*slot != EMPTY).then(|| std::mem::replace(slot, EMPTY) as usize)
    }

    pub(super) fn clear(&mut self) {
        if let Some(slots) = &mut self.0 {
            slots.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::collection_data::CollectionData;

    fn number(data: &CollectionData, key: f64) -> Option<f64> {
        match data.lookup(&Value::Num(key)) {
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        }
    }

    #[test]
    fn sparse_hash_entries_are_found_after_the_dense_frontier_passes_them() {
        let mut data = CollectionData::default();
        data.insert(Value::Num(1000.0), Value::Num(17.0));
        for n in 0..1100 {
            if n != 1000 {
                data.insert(Value::Num(n as f64), Value::Num(n as f64));
            }
        }
        assert_eq!(number(&data, 1000.0), Some(17.0));
        data.insert(Value::Num(1000.0), Value::Num(23.0));
        assert_eq!(data.len(), 1100);
        assert_eq!(number(&data, 1000.0), Some(23.0));
        assert!(data.remove(&Value::Num(1000.0)));
        assert_eq!(number(&data, 1000.0), None);
        data.insert(Value::Num(1000.0), Value::Num(29.0));
        assert_eq!(number(&data, 1000.0), Some(29.0));
        assert!(matches!(
            data.iter().last(),
            Some((Value::Num(1000.0), Value::Num(29.0)))
        ));
    }

    #[test]
    fn mixed_keys_keep_same_value_zero_and_insertion_order() {
        let mut data = CollectionData::default();
        for key in [
            Value::Num(-0.0),
            Value::str("0"),
            Value::Bool(false),
            Value::Num(0.5),
            Value::Num(-1.0),
            Value::Num(f64::NAN),
            Value::Num(f64::INFINITY),
        ] {
            data.insert(key, Value::Num(data.len() as f64));
        }
        assert_eq!(data.len(), 7);
        assert_eq!(number(&data, 0.0), Some(0.0));
        assert_eq!(number(&data, f64::NAN), Some(5.0));
        assert_eq!(number(&data, 0.5), Some(3.0));
        let mut cursor = 0;
        assert!(matches!(data.next(&mut cursor), Some((Value::Num(n), _)) if n.to_bits()==0));
        assert!(data.remove(&Value::Num(-0.0)));
        data.insert(Value::Num(0.0), Value::Num(11.0));
        assert!(matches!(
            data.iter().last(),
            Some((Value::Num(0.0), Value::Num(11.0)))
        ));
        assert!(matches!(data.next(&mut cursor), Some((Value::Str(text), _)) if &**text == "0"));
    }

    #[test]
    fn clear_and_compaction_rebuild_integer_slots_without_stale_references() {
        let mut data = CollectionData::default();
        for n in 0..20 {
            data.insert(Value::Num(n as f64), Value::Num(n as f64));
        }
        data.insert(Value::str("stable"), Value::Num(77.0));
        // The shared storage helper remains correct even when compaction sees numeric keys.
        for n in 0..15 {
            assert!(data.remove_weak(&Value::Num(n as f64)));
        }
        for n in 15..20 {
            assert_eq!(number(&data, n as f64), Some(n as f64));
        }
        assert!(matches!(
            data.lookup(&Value::str("stable")),
            Some(Value::Num(77.0))
        ));
        let mut cursor = 0;
        assert!(data.next(&mut cursor).is_some());
        data.clear();
        data.insert(Value::Num(0.0), Value::Num(99.0));
        assert!(matches!(
            data.next(&mut cursor),
            Some((Value::Num(0.0), Value::Num(99.0)))
        ));
        assert_eq!(number(&data, 19.0), None);
    }

    #[test]
    fn sparse_and_large_keys_do_not_allocate_unbounded_direct_storage() {
        let mut index = DenseIndex::default();
        assert!(index.candidate(&Value::Num(1000.0), 0).is_none());
        assert!(index.0.is_none());
        for n in 0..LIMIT {
            index.insert(n, n);
        }
        assert!(index.candidate(&Value::Num(LIMIT as f64), 0).is_none());
        assert_eq!(index.candidate(&Value::Num(0.0), EMPTY as usize), Some(0));
        assert!(DenseIndex::default()
            .candidate(&Value::Num(0.0), EMPTY as usize)
            .is_none());
        assert_eq!(index.0.as_ref().unwrap().len(), LIMIT);
        assert!(index.0.as_ref().unwrap().capacity() <= LIMIT);
        assert_eq!(index.lookup(&Value::Num(-0.0)), Some(0));
        assert!(index.lookup(&Value::Num(f64::NAN)).is_none());
    }
}
