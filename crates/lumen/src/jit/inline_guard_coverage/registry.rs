//! Copied compile records never retain an engine object or environment.
use crate::bytecode::{Chunk, Op};
use crate::value::Callable;
use std::rc::Rc;
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(super) fn unique_id() -> u64 {
    NEXT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("diagnostic ID space exhausted")
}
struct Guard {
    id: u64,
    pc: usize,
    description: String,
    emitted: Cell<bool>,
}
pub(in crate::jit) struct Compilation {
    id: u64,
    guards: Vec<Guard>,
    installed: Cell<bool>,
}
impl Compilation {
    pub(in crate::jit) fn begin(chunk: &Chunk) -> Option<Self> {
        std::env::var_os("LUMEN_JIT_INLINE_GUARD_COVERAGE")?;
        let id = unique_id();
        let mut guards = Vec::new();
        for (pc, op) in chunk.jit_ops().iter().enumerate() {
            let Op::InlineGuard(t, target) = op else {
                continue;
            };
            let it = chunk.jit_inline_target(*t);
            let adjacent_method = pc > 0 && matches!(chunk.jit_ops()[pc - 1], Op::GetMethod(..));
            let callee = describe_callee(it);
            guards.push(Guard { id: unique_id(), pc, emitted: Cell::new(false),
                description: format!("target={t} fallback={target} argc={} check_this={} expected_env={} expected={} pin_live={} adjacent_method={adjacent_method} {callee}",it.argc,it.check_this,it.expected_env,it.expected,it.pin.strong_count()>0) });
        }
        if !guards.is_empty() {
            eprintln!("[inline-guard-compile] id={id} chunk={} ops={} slots={:?} coverage=ordinary-only region-bypass=unknown", chunk as *const Chunk as usize,chunk.jit_ops().len(),chunk.jit_slot_names());
            for (pc, op) in chunk.jit_ops().iter().enumerate() {
                eprintln!("[inline-guard-op] compile={id} pc={pc} {op:?}");
            }
        }
        Some(Self {
            id,
            guards,
            installed: Cell::new(false),
        })
    }
    pub(in crate::jit) fn ordinary(&self, pc: usize) -> Option<u64> {
        let guard = self.guards.iter().find(|guard| guard.pc == pc)?;
        guard.emitted.set(true);
        Some(guard.id)
    }
    pub(in crate::jit) fn installed(&self) {
        self.installed.set(true);
    }
}
impl Drop for Compilation {
    fn drop(&mut self) {
        for guard in &self.guards {
            eprintln!(
                "[inline-guard-site] compile={} id={} pc={} ordinary={} installed={} {}",
                self.id,
                guard.id,
                guard.pc,
                guard.emitted.get(),
                self.installed.get(),
                guard.description
            );
        }
    }
}

fn describe_callee(target: &crate::bytecode::InlineTarget) -> String {
    let Some(object) = target.pin.upgrade() else {
        return "callee_object=0 callee_function=0".into();
    };
    let object_id = Rc::as_ptr(&object) as usize;
    let Ok(borrowed) = object.try_borrow() else {
        return format!("callee_object={object_id} callee_borrowed=true");
    };
    let Callable::User(user) = &borrowed.call else {
        return format!("callee_object={object_id} callee_user=false");
    };
    format!(
        "callee_object={object_id} callee_function={} callee_source={:?}",
        Rc::as_ptr(&user.func) as usize,
        user.func.source.as_deref()
    )
}
