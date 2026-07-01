//! IANA time-zone lookups over the generated [`crate::tzdata`] offset tables.

use crate::tzdata::{Zone, LINKS, ZONES};

/// The canonical registry name for a case-insensitive IANA zone id (`None` if unknown).
pub fn canonicalize(name: &str) -> Option<&'static str> {
    if let Some(z) = ZONES.iter().find(|z| z.name.eq_ignore_ascii_case(name)) {
        return Some(z.name);
    }
    LINKS
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(name))
        .map(|(_, canon)| *canon)
}

fn zone(name: &str) -> Option<&'static Zone> {
    let canon = canonicalize(name)?;
    ZONES.iter().find(|z| z.name == canon)
}

/// The UTC offset (seconds) in effect at `epoch_sec` for a named zone.
pub fn offset_at(name: &str, epoch_sec: i64) -> Option<i32> {
    let z = zone(name)?;
    let idx = z.transitions.partition_point(|&(t, _)| t <= epoch_sec);
    Some(if idx == 0 { z.initial } else { z.transitions[idx - 1].1 })
}

/// The epoch-second of the next (`forward`) or previous offset transition strictly after/before
/// `epoch_sec`, or `None` when the zone has no further transition in that direction.
pub fn next_transition(name: &str, epoch_sec: i64, forward: bool) -> Option<i64> {
    let z = zone(name)?;
    let ts = z.transitions;
    if forward {
        ts.iter().find(|&&(t, _)| t > epoch_sec).map(|&(t, _)| t)
    } else {
        ts.iter().rev().find(|&&(t, _)| t < epoch_sec).map(|&(t, _)| t)
    }
}
