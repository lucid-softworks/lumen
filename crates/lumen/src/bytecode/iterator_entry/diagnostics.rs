//! Opt-in feedback counters, including diagnostic-only preparation time.
use std::{cell::RefCell, collections::BTreeMap};

pub(super) struct Counts(pub(super) BTreeMap<String, u64>);
impl Drop for Counts {
    fn drop(&mut self) {
        for (status, count) in &self.0 {
            eprintln!("[iterator-entry-feedback] {count} {status}");
        }
    }
}
thread_local! {pub(super) static COUNTS:RefCell<Counts>=const {RefCell::new(Counts(BTreeMap::new()))};}
pub(super) fn record(status: &str) {
    record_amount(status, 1);
}
pub(super) fn record_amount(status: &str, amount: u64) {
    let _ = COUNTS.try_with(|counts| {
        let mut counts = counts.borrow_mut();
        if let Some(count) = counts.0.get_mut(status) {
            *count += amount;
        } else {
            counts.0.insert(status.into(), amount);
        }
    });
}
