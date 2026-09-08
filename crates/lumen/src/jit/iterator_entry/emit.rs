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
    /// Rebind only metadata owned by this exact selected Chunk. All fallible
    /// preparation finishes before the first old guard or cache is changed.
    pub(super) fn rebind(&mut self, chunk: &Rc<Chunk>, env: &Env) -> bool {
        if self.source.as_ptr() != Rc::as_ptr(chunk) {
            return false;
        }
        let mut prepared = Vec::with_capacity(self._names.len());
        for guard in &self._names {
            prepared.push(match guard {
                Some(guard) => match names::prepare_rebind(guard, env) {
                    Some(replacement) => Some(replacement),
                    None => return false,
                },
                None => None,
            });
        }
        for (guard, replacement) in self._names.iter_mut().zip(prepared) {
            if let (Some(guard), Some(replacement)) = (guard, replacement) {
                names::commit_rebind(guard, replacement);
            }
        }
        for cache in &self.caches {
            copy_cache(chunk, cache);
        }
        self.refresh.set(false);
        true
    }
    pub(super) fn code_len(&self) -> usize {
        self.len
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ast::Stmt,
        bytecode,
        interpreter::{new_scope, Binding},
        jit_ir::iterator_entry,
        parser, Engine,
    };

    fn chunk() -> Rc<Chunk> {
        let parsed = parser::parse_script(
            "function next(){return {value:first+second,done:false};}",
            false,
        )
        .unwrap_or_else(|_| panic!("fixture syntax"));
        let Stmt::FuncDecl(function) = &parsed[0] else {
            panic!("function")
        };
        bytecode::compile(function).expect("compiled fixture")
    }
    fn environment(engine: &Engine, first: f64, second: Option<f64>) -> Env {
        let env = new_scope(Some(engine.interp.global_env.clone()));
        env.borrow_mut()
            .vars
            .insert("first", Binding::data(Value::Num(first), true, true));
        if let Some(second) = second {
            env.borrow_mut()
                .vars
                .insert("second", Binding::data(Value::Num(second), true, true));
        }
        env
    }
    #[test]
    fn rebind_preserves_code_and_releases_old_captured_environment() {
        let mut engine = Engine::new();
        let chunk = chunk();
        let plan = iterator_entry::analyze(&chunk).unwrap();
        let old = environment(&engine, 7.0, Some(11.0));
        let old_weak = Rc::downgrade(&old);
        let mut native = compile(&plan, &chunk, &old).expect("native code");
        let memory = native.mem;
        let iterator = Value::Obj(Object::new(None));
        assert!(matches!(
            unsafe { native.run(&mut engine.interp, &old, &iterator) },
            Some(Value::Num(18.0))
        ));
        let fresh = environment(&engine, 19.0, Some(23.0));
        assert!(native.rebind(&chunk, &fresh));
        assert_eq!(native.mem, memory);
        assert!(matches!(
            unsafe { native.run(&mut engine.interp, &fresh, &iterator) },
            Some(Value::Num(42.0))
        ));
        assert!(unsafe { native.run(&mut engine.interp, &old, &iterator) }.is_none());
        drop(old);
        assert!(old_weak.upgrade().is_none());
        let weak_chunk = Rc::downgrade(&chunk);
        drop(chunk);
        assert!(weak_chunk.upgrade().is_none());
    }
    #[test]
    fn failed_late_name_preparation_and_other_chunk_preserve_old_entry() {
        let mut engine = Engine::new();
        let source = chunk();
        let plan = iterator_entry::analyze(&source).unwrap();
        let old = environment(&engine, 2.0, Some(3.0));
        let mut native = compile(&plan, &source, &old).unwrap();
        let memory = native.mem;
        let iterator = Value::Obj(Object::new(None));
        let incomplete = environment(&engine, 100.0, None);
        assert!(!native.rebind(&source, &incomplete));
        assert_eq!(native.mem, memory);
        assert!(matches!(
            unsafe { native.run(&mut engine.interp, &old, &iterator) },
            Some(Value::Num(5.0))
        ));
        let other = chunk();
        let fresh = environment(&engine, 10.0, Some(20.0));
        assert!(!native.rebind(&other, &fresh));
        assert_eq!(native.mem, memory);
        assert!(matches!(
            unsafe { native.run(&mut engine.interp, &old, &iterator) },
            Some(Value::Num(5.0))
        ));
    }
}
