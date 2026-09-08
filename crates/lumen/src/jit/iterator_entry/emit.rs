//! A standalone ARM64 frame with borrowed SSA slots and one deferred commit.
mod operations;
use super::{names, values};
use crate::{
    bytecode::{Chunk, IcState, PROP_IC_WAYS},
    interpreter::{Env, Interp},
    jit::{asm::Asm, sys},
    jit_ir::iterator_entry::{Candidate, Expr},
    value::{jit_layout, Object, Value},
};
use std::{
    cell::Cell,
    mem::MaybeUninit,
    rc::{Rc, Weak},
};

struct Cache {
    index: u32,
    ways: Box<[Cell<IcState>; PROP_IC_WAYS]>,
}
pub(super) struct Native {
    mem: *mut u8,
    len: usize,
    source: Weak<Chunk>,
    caches: Vec<Cache>,
    _names: Vec<Option<names::Guard>>,
    refresh: Cell<bool>,
}
impl Drop for Native {
    fn drop(&mut self) {
        unsafe { sys::free_exec(self.mem, self.len) }
    }
}
impl Native {
    pub(super) unsafe fn run(&self, i: &mut Interp, env: &Env, iterator: &Value) -> Option<Value> {
        if self.refresh.replace(false) {
            let chunk = self.source.upgrade()?;
            for cache in &self.caches {
                copy_cache(&chunk, cache);
            }
        }
        let mut output = MaybeUninit::uninit();
        let function: unsafe extern "C" fn(
            *mut Interp,
            *const std::cell::RefCell<crate::interpreter::Scope>,
            *const Value,
            *mut Value,
        ) -> u32 = std::mem::transmute(self.mem);
        if function(i, Rc::as_ptr(env), iterator, output.as_mut_ptr()) == 1 {
            Some(output.assume_init())
        } else {
            self.refresh.set(true);
            None
        }
    }
}
fn copy_cache(chunk: &Chunk, cache: &Cache) {
    let source = chunk.jit_cache_ptr(cache.index) as *const Cell<IcState>;
    for (way, target) in cache.ways.iter().enumerate() {
        target.set(unsafe { (&*source.add(way)).get() });
    }
}
pub(super) fn compile(plan: &Candidate, chunk: &Rc<Chunk>, env: &Env) -> Option<Native> {
    if plan.expressions.is_empty() || plan.expressions.len() > 128 || plan.return_pc >= 64 {
        return None;
    }
    let sample = Object::new(None);
    let layout = jit_layout(&sample);
    if !names::object_borrow_supported(&layout)
        || !super::element::supported(&layout)
        || !crate::jit::get_prop_inlinable(&layout)
        || !crate::jit::packed_elem_inlinable(&layout)
    {
        return None;
    }
    let mut caches = Vec::new();
    let mut guards = Vec::new();
    for expression in &plan.expressions {
        guards.push(if let Expr::Name { name, .. } = expression {
            Some(names::prepare(env, name)?)
        } else {
            None
        });
        if let Expr::Own { cache, name, .. } = expression {
            if !cache_matches(chunk, *cache, name, false) {
                return None;
            }
            add_cache(&mut caches, chunk, *cache);
        }
    }
    if let Some(store) = &plan.store {
        if crate::value::canonical_index(&store.name).is_some() {
            return None;
        }
        if !cache_matches(chunk, store.cache, &store.name, true) {
            return None;
        }
        add_cache(&mut caches, chunk, store.cache);
    }
    let mut a = Asm::new();
    let fail = a.new_label();
    let done = a.new_label();
    let frame = ((plan.expressions.len() * 16 + 32 + 15) & !15) as u32;
    prologue(&mut a, frame);
    let mut context = operations::Context {
        a: &mut a,
        layout: &layout,
        plan,
        caches: &caches,
        guards: &guards,
        fail,
        prepared: false,
    };
    for (id, expr) in plan.expressions.iter().enumerate() {
        context.expression(id, expr)?;
    }
    context.finish()?;
    a.movz(0, 1, 0);
    a.b(done);
    a.bind(fail);
    a.movz(0, 0, 0);
    a.bind(done);
    epilogue(&mut a, frame);
    let code = a.finish();
    let len = code.len() * 4;
    let mem = unsafe { sys::alloc_exec(code.as_ptr().cast(), len) };
    if mem.is_null() {
        return None;
    }
    Some(Native {
        mem,
        len,
        source: Rc::downgrade(chunk),
        caches,
        _names: guards,
        refresh: Cell::new(false),
    })
}
fn add_cache(caches: &mut Vec<Cache>, chunk: &Chunk, index: u32) {
    if caches.iter().any(|c| c.index == index) {
        return;
    }
    let cache = Cache {
        index,
        ways: Box::new(std::array::from_fn(|_| Cell::new(IcState::EMPTY))),
    };
    copy_cache(chunk, &cache);
    caches.push(cache);
}
fn prologue(a: &mut Asm, frame: u32) {
    a.stp_pre(29, 30, -16);
    a.stp_pre(19, 20, -16);
    a.stp_pre(21, 22, -16);
    a.stp_pre(23, 24, -16);
    a.sub_imm(31, 31, frame);
    a.add_imm(23, 31, 0);
    a.mov(19, 0);
    a.mov(20, 1);
    a.mov(22, 2);
    a.mov(21, 3);
}
fn epilogue(a: &mut Asm, frame: u32) {
    a.add_imm(31, 31, frame);
    a.ldp_post(23, 24, 16);
    a.ldp_post(21, 22, 16);
    a.ldp_post(19, 20, 16);
    a.ldp_post(29, 30, 16);
    a.ret();
}

// A matching compiler-owned opcode proves the four-way range exists and belongs
// to this exact property name. Never trust an arbitrary Candidate cache index.
fn cache_matches(chunk: &Chunk, index: u32, name: &str, store: bool) -> bool {
    use crate::bytecode::Op;
    chunk.jit_ops().iter().any(|op| match *op {
        Op::GetProp(n, c) | Op::GetPropThis(n, c) if !store => {
            c == index && chunk.jit_name(n) == name
        }
        Op::SetPropDrop(n, c) if store => c == index && chunk.jit_name(n) == name,
        _ => false,
    })
}
