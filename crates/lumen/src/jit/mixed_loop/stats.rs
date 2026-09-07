//! Opt-in native coverage diagnostics. Instrumented timings are not performance evidence.
use crate::{bytecode::Chunk, jit::asm::Asm};
use std::{
    cell::{Cell, RefCell},
    io::Write,
};

struct Counters {
    id: usize,
    head: usize,
    slots: String,
    entries: Cell<usize>,
    backward_jumps: Cell<usize>,
    exits: Box<[Cell<usize>]>,
}

#[derive(Default)]
struct Registry(Vec<&'static Counters>);

thread_local! {
    static REGISTRY: RefCell<Registry> = const { RefCell::new(Registry(Vec::new())) };
}

impl Drop for Registry {
    fn drop(&mut self) {
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = writeln!(
            out,
            "[mixed-stats] diagnostic only; instrumented timings invalid; thread-teardown snapshot"
        );
        for counters in &self.0 {
            let _ = writeln!(
                out,
                "[mixed-stats] plan={} head={} slots={:?} entries={} backward_jumps={}",
                counters.id,
                counters.head,
                counters.slots,
                counters.entries.get(),
                counters.backward_jumps.get()
            );
            for (pc, count) in counters.exits.iter().enumerate() {
                if count.get() != 0 {
                    let _ = writeln!(
                        out,
                        "[mixed-stats-exit] plan={} pc={} count={}",
                        counters.id,
                        pc,
                        count.get()
                    );
                }
            }
        }
    }
}

/// Native counters follow this engine's existing compile/execute-on-the-same-thread contract.
/// Allocations intentionally survive TLS teardown: generated code may outlive the registry,
/// and destructor order must never make embedded addresses dangling. Enabled diagnostics leak
/// one bounded counter allocation per compiled plan until process exit. Default allocates none.
pub(super) struct Stats(Option<&'static Counters>);

impl Stats {
    pub(super) fn new(chunk: &Chunk, head: usize) -> Self {
        if std::env::var_os("LUMEN_JIT_MIXED_STATS").is_none() {
            return Self(None);
        }
        let counters = REGISTRY
            .try_with(|registry| {
                let mut registry = registry.borrow_mut();
                let counters = Box::leak(Box::new(Counters {
                    id: registry.0.len(),
                    head,
                    slots: chunk
                        .jit_slot_names()
                        .iter()
                        .map(|s| s.as_ref())
                        .collect::<Vec<&str>>()
                        .join("|"),
                    entries: Cell::new(0),
                    backward_jumps: Cell::new(0),
                    exits: (0..chunk.jit_ops().len()).map(|_| Cell::new(0)).collect(),
                }));
                registry.0.push(counters);
                &*counters
            })
            .ok();
        Self(counters)
    }

    pub(super) fn entry(&self, a: &mut Asm) {
        if let Some(c) = self.0 {
            increment(a, &c.entries);
        }
    }

    /// Counts executed backwards jumps, including the jump that exhausts the budget.
    pub(super) fn backward_jump(&self, a: &mut Asm) {
        if let Some(c) = self.0 {
            increment(a, &c.backward_jumps);
        }
    }

    /// Aggregates guard, unsupported-op and budget exits by exact resume PC.
    pub(super) fn exit(&self, a: &mut Asm, pc: usize) {
        if let Some(c) = self.0 {
            increment(a, &c.exits[pc]);
        }
    }
}

fn increment(a: &mut Asm, counter: &Cell<usize>) {
    a.mov_imm64(9, counter.as_ptr() as usize as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}

#[cfg(test)]
mod tests {
    #[test]
    fn disabled_handle_emits_no_instructions() {
        let mut a = crate::jit::asm::Asm::new();
        let stats = super::Stats(None);
        stats.entry(&mut a);
        stats.backward_jump(&mut a);
        stats.exit(&mut a, usize::MAX);
        assert!(a.finish().is_empty());
    }
}
