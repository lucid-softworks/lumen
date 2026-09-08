//! Prepare complete weak lexical replacements before publishing stable metadata.
use super::{prepare, Guard};
use crate::interpreter::Env;

/// Prepared independently; committing requires exclusive access and no native invocation.
pub(in crate::jit::iterator_entry) struct Prepared(Guard);

pub(in crate::jit::iterator_entry) fn prepare_rebind(old: &Guard, env: &Env) -> Option<Prepared> {
    let replacement = prepare(env, &old.name)?;
    (replacement.scopes.len() == old.scopes.len()
        && replacement.resolver.is_some() == old.resolver.is_some())
    .then_some(Prepared(replacement))
}

/// Caller prepares every name before committing any. The executable's embedded metadata
/// address never changes. Replacement owners exist before old weak descriptors are dropped.
pub(in crate::jit::iterator_entry) fn commit_rebind(old: &mut Guard, prepared: Prepared) {
    let mut replacement = prepared.0;
    std::mem::swap(&mut *old.metadata, &mut *replacement.metadata);
    std::mem::swap(&mut old.scopes, &mut replacement.scopes);
    std::mem::swap(&mut old.resolver, &mut replacement.resolver);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::{Binding, Scope};
    use crate::jit::asm::Asm;
    use crate::jit::iterator_entry::names::{emit, prepare, Metadata};
    use crate::value::Value;
    use crate::{
        interpreter::new_scope,
        jit::{sys, JitCode},
    };
    use std::{cell::RefCell, rc::Rc};

    fn scope(value: f64) -> Env {
        let env = new_scope(None);
        env.borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(value), true, true));
        env
    }

    #[test]
    fn emitted_rebind_keeps_code_and_metadata_but_releases_old_scope() {
        let original = scope(7.0);
        let weak = Rc::downgrade(&original);
        let mut guard = prepare(&original, "x").unwrap();
        let address = &*guard.metadata as *const Metadata;
        let mut a = Asm::new();
        let fail = a.new_label();
        let done = a.new_label();
        a.stp_pre(29, 30, -16);
        emit(&mut a, &guard, 0, fail);
        a.mov(0, 14);
        a.b(done);
        a.bind(fail);
        a.movz(0, 0, 0);
        a.bind(done);
        a.ldp_post(29, 30, 16);
        a.ret();
        let words = a.finish();
        let len = words.len() * 4;
        let mem = unsafe { sys::alloc_exec(words.as_ptr().cast(), len) };
        assert!(!mem.is_null());
        let code = JitCode {
            mem,
            len,
            pc_offsets: Vec::new(),
            max_stack: 0,
            needs_global: false,
        };
        let run: unsafe extern "C" fn(*const RefCell<Scope>) -> *const Value =
            unsafe { std::mem::transmute(code.mem_ptr()) };
        let check = |env: &Env, expected: f64| {
            let value = unsafe { run(Rc::as_ptr(env)) };
            assert!(!value.is_null());
            assert!(matches!(unsafe { &*value }, Value::Num(n) if *n == expected));
        };
        check(&original, 7.0);
        let replacement = scope(23.0);
        let pending = prepare_rebind(&guard, &replacement).unwrap();
        check(&original, 7.0); // preparation cannot publish partial changes
        assert!(unsafe { run(Rc::as_ptr(&replacement)) }.is_null());
        commit_rebind(&mut guard, pending);
        assert_eq!(address, &*guard.metadata as *const Metadata);
        check(&replacement, 23.0);
        assert!(unsafe { run(Rc::as_ptr(&original)) }.is_null());
        drop(original);
        assert!(weak.upgrade().is_none());
        let deeper = new_scope(Some(replacement.clone()));
        assert!(prepare_rebind(&guard, &deeper).is_none());
        check(&replacement, 23.0);
        replacement
            .borrow_mut()
            .vars
            .get_mut("x")
            .unwrap()
            .initialized = false;
        assert!(unsafe { run(Rc::as_ptr(&replacement)) }.is_null());
    }
}
