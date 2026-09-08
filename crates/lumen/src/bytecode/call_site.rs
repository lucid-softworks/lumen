//! Identity call caches and bounded pins for refreshed closure instances.
use super::{CallIc, CALL_IC_WAYS};
use crate::value::Gc;
use std::rc::Rc;

pub struct CallSite {
    pub entries: [std::cell::Cell<CallIc>; CALL_IC_WAYS],
    /// Round-robin fill cursor.
    pub next: std::cell::Cell<u8>,
}

impl super::Chunk {
    pub(crate) fn call_site(&self, index: u32) -> &CallSite {
        &self.call_caches[index as usize]
    }

    pub(crate) fn refresh_call_cache(&self, index: u32, ic: CallIc, callee: &Gc) {
        self.call_site(index).refresh(
            ic,
            callee,
            self.call_refresh_pins
                .borrow_mut()
                .entry(index)
                .or_default(),
        );
    }
}

impl CallSite {
    pub(super) fn seeded(entries: [CallIc; CALL_IC_WAYS], next: u8) -> Self {
        Self {
            entries: entries.map(std::cell::Cell::new),
            next: std::cell::Cell::new(next),
        }
    }

    /// Publish an already validated closure retry for subsequent identity probes.
    /// One replaceable Weak per site prevents the current address from being recycled
    /// without consuming the chunk's lifetime pin budget. Before releasing the previous
    /// pin, erase every entry that could depend on it. No callback runs during this swap.
    ///
    /// Refreshed identities are deliberately absent from Chunk::call_pins: optimization
    /// discovery and cache seeding must not bake pointers whose pin we can later release.
    pub(super) fn refresh(&self, ic: CallIc, callee: &Gc, pin: &mut Option<super::CallPin>) {
        debug_assert_eq!(ic.callee, Rc::as_ptr(callee) as usize);
        let replacement = Rc::downgrade(callee);
        let previous = pin.as_ref().map(|old| old.as_ptr() as usize);
        let slot = previous
            .and_then(|key| self.entries.iter().position(|e| e.get().callee == key))
            .or_else(|| {
                self.entries.iter().position(|e| {
                    let entry = e.get();
                    entry.func == ic.func && entry.native == 0
                })
            })
            .unwrap_or(self.next.get() as usize & (CALL_IC_WAYS - 1));
        if let Some(key) = previous {
            for entry in &self.entries {
                if entry.get().callee == key {
                    entry.set(CallIc::EMPTY);
                }
            }
        }
        *pin = Some(replacement);
        self.entries[slot].set(ic);
    }

    pub fn empty() -> CallSite {
        CallSite {
            entries: [
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
                std::cell::Cell::new(CallIc::EMPTY),
            ],
            next: std::cell::Cell::new(0),
        }
    }
    /// Record `ic`, replacing an existing way for the same callee (epoch or realm refills must
    /// not fan one callee across ways — the inline planner reads way-count as polymorphism),
    /// else the next way round-robin.
    pub fn fill(&self, ic: CallIc) {
        for e in &self.entries {
            if e.get().callee == ic.callee {
                e.set(ic);
                return;
            }
        }
        let k = self.next.get() as usize & 3;
        self.entries[k].set(ic);
        self.next.set((k as u8 + 1) & 3);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;

    fn entry(callee: &Gc) -> CallIc {
        CallIc {
            callee: Rc::as_ptr(callee) as usize,
            ..CallIc::EMPTY
        }
    }

    #[test]
    fn replacing_a_refresh_erases_old_identity_before_releasing_its_pin() {
        let site = CallSite::empty();
        let mut pin = None;
        let first = Object::new(None);
        site.refresh(entry(&first), &first, &mut pin);
        assert_eq!(Rc::weak_count(&first), 1);
        // Include a duplicated entry to check every possible stale identity is removed.
        site.entries[3].set(entry(&first));
        let second = Object::new(None);
        site.refresh(entry(&second), &second, &mut pin);
        assert_eq!(Rc::weak_count(&first), 0);
        assert_eq!(Rc::weak_count(&second), 1);
        assert!(!site
            .entries
            .iter()
            .any(|e| e.get().callee == Rc::as_ptr(&first) as usize));
        assert_eq!(
            site.entries.iter().filter(|e| e.get().callee != 0).count(),
            1
        );
    }

    #[test]
    fn refresh_pin_is_bounded_after_ordinary_cache_eviction() {
        let site = CallSite::empty();
        let mut pin = None;
        let first = Object::new(None);
        site.refresh(entry(&first), &first, &mut pin);
        let permanent: Vec<_> = (0..CALL_IC_WAYS).map(|_| Object::new(None)).collect();
        for object in &permanent {
            site.fill(entry(object));
        }
        let mut previous = first;
        for _ in 0..5000 {
            let current = Object::new(None);
            site.refresh(entry(&current), &current, &mut pin);
            assert_eq!(Rc::weak_count(&previous), 0);
            assert_eq!(Rc::weak_count(&current), 1);
            previous = current;
        }
        drop(pin);
        assert_eq!(Rc::weak_count(&previous), 0);
    }
}
