//! Weak-owned exact lexical paths from an explicit, strongly rooted definition environment.
mod layout;
use crate::interpreter::{Binding, Env, Scope};
use crate::jit::{asm::Asm, C_MI, C_NE};
use crate::value::Value;
use std::cell::RefCell;
use std::rc::{Rc, Weak};

struct ScopeGuard {
    identity: Weak<RefCell<Scope>>,
    generation: u32,
}

/// Retain this descriptor as long as code containing its identities/binding pointer exists.
/// Weak owners prevent identity reuse without rooting any captured environment graph.
pub(super) struct Guard {
    scopes: Vec<ScopeGuard>,
    name: String,
    metadata: Box<Metadata>,
    resolver: Option<Box<Resolver>>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScopeMetadata {
    identity: usize,
    generation: u64,
}

#[repr(C)]
struct Metadata {
    scopes: [ScopeMetadata; 8],
    argument: usize,
}

struct Resolver {
    name: String,
    scopes: Vec<Weak<RefCell<Scope>>>,
}

pub(super) fn prepare(env: &Env, name: &str) -> Option<Guard> {
    if !supported() {
        return None;
    }
    let mut current = env.clone();
    let mut scopes = Vec::new();
    for _ in 0..8 {
        let scope = current.try_borrow().ok()?;
        if scope.with_obj.is_some() || scope.under_with {
            return None;
        }
        scopes.push(ScopeGuard {
            identity: Rc::downgrade(&current),
            generation: scope.vars.generation(),
        });
        if let Some(binding) = scope.vars.get(name) {
            if !binding.initialized
                || binding.import_ref.is_some()
                || matches!(binding.value, Value::Empty)
            {
                return None;
            }
            let resolver = scopes.iter().any(|scope| scope.generation != 0).then(|| {
                Box::new(Resolver {
                    name: name.to_owned(),
                    scopes: scopes.iter().map(|scope| scope.identity.clone()).collect(),
                })
            });
            let mut metadata = Box::new(Metadata {
                scopes: [ScopeMetadata::default(); 8],
                argument: resolver
                    .as_ref()
                    .map_or(binding as *const Binding as usize, |resolver| {
                        (&**resolver as *const Resolver) as usize
                    }),
            });
            for (target, scope) in metadata.scopes.iter_mut().zip(&scopes) {
                *target = ScopeMetadata {
                    identity: scope.identity.as_ptr() as usize,
                    generation: u64::from(scope.generation),
                };
            }
            return Some(Guard {
                scopes,
                name: name.to_owned(),
                metadata,
                resolver,
            });
        }
        let parent = scope.parent.clone()?;
        drop(scope);
        current = parent;
    }
    None
}

fn supported() -> bool {
    let l = layout::layout();
    l.valid
        && l.borrow_flag.is_multiple_of(8)
        && l.borrow_flag < 32768
        && l.parent.is_multiple_of(8)
        && l.parent < 32768
        && l.generation.is_multiple_of(4)
        && l.generation < 16384
        && l.under_with < 4096
        && l.rc_data_offset < 4096
}

/// The same compact RefCell borrow prefix is valid for Object when its measured value
/// begins one machine word after Rc::as_ptr. Consumers must still reject a negative flag.
pub(super) fn object_borrow_supported(object: &crate::value::JitLayout) -> bool {
    layout::layout().valid
        && object.obj_from_rc.checked_sub(object.gc_data_off) == Some(std::mem::size_of::<isize>())
}

/// Return x14 pointing at a live wide Value. Input env_reg is Rc::as_ptr(Env), not stored Rc.
/// Clobbers all caller-saved GPR/FP registers via checked_binding; preserve live SSA homes
/// in the caller's native spill frame. x19..x29/SP remain intact. Caller saves LR normally.
/// All guards and the bounded helper precede any output ownership or pending heap commit.
pub(super) fn emit(a: &mut Asm, guard: &Guard, env_reg: u32, fail: usize) {
    assert!(supported());
    let l = layout::layout();
    a.mov(9, env_reg);
    a.mov_imm64(12, (&*guard.metadata as *const Metadata) as usize as u64);
    for index in 0..guard.scopes.len() {
        let record = (index * std::mem::size_of::<ScopeMetadata>()) as u32;
        a.ldr_imm(10, 12, record);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, fail);
        a.ldr_imm(10, 9, l.borrow_flag as u32);
        a.cmp_imm_x(10, 0);
        a.b_cond(C_MI, fail);
        a.ldrb_imm(10, 9, l.under_with as u32);
        a.cbnz(10, false, fail);
        a.ldr_w_imm(10, 9, l.generation as u32);
        a.ldr_imm(
            11,
            12,
            record + std::mem::offset_of!(ScopeMetadata, generation) as u32,
        );
        a.cmp_reg_w(10, 11);
        a.b_cond(C_NE, fail);
        if index + 1 != guard.scopes.len() {
            a.ldr_imm(9, 9, l.parent as u32);
            a.cbz(9, true, fail);
            a.add_imm(9, 9, l.rc_data_offset as u32);
        }
    }
    a.ldr_imm(0, 12, std::mem::offset_of!(Metadata, argument) as u32);
    if guard.resolver.is_some() {
        a.mov_imm64(16, resolve_binding as *const () as usize as u64);
    } else {
        // Structural mutation never restores generation zero, so this allocation cannot
        // have moved after preparing a path whose every generation remains zero.
        a.mov_imm64(16, checked_binding as *const () as usize as u64);
    }
    a.blr(16);
    a.cbz(0, true, fail);
    a.mov(14, 0);
}

/// Re-resolve every name position instead of trusting a wrapping nonzero generation.
/// Weak upgrades pin the complete live chain; no callback, GC poll, or mutation occurs.
/// The caller's rooted definition environment retains the returned binding after we return.
unsafe extern "C" fn resolve_binding(descriptor: *const Resolver) -> *const Value {
    let descriptor = unsafe { &*descriptor };
    for (index, expected) in descriptor.scopes.iter().enumerate() {
        let Some(owner) = expected.upgrade() else {
            return std::ptr::null();
        };
        let Ok(scope) = owner.try_borrow() else {
            return std::ptr::null();
        };
        if scope.under_with || scope.with_obj.is_some() {
            return std::ptr::null();
        }
        if index + 1 == descriptor.scopes.len() {
            return scope
                .vars
                .get(&descriptor.name)
                .map_or(std::ptr::null(), |binding| unsafe {
                    checked_binding(binding)
                });
        }
        if scope.vars.get(&descriptor.name).is_some()
            || !scope
                .parent
                .as_ref()
                .is_some_and(|parent| Rc::as_ptr(parent) == descriptor.scopes[index + 1].as_ptr())
        {
            return std::ptr::null();
        }
    }
    std::ptr::null()
}

/// # Safety
/// The complete live scope path and final generation have just been validated without
/// mutation/reentry and either every generation is zero or this pointer was freshly
/// resolved by name. Its original env owner roots the final Binding allocation throughout.
unsafe extern "C" fn checked_binding(binding: *const Binding) -> *const Value {
    let binding = unsafe { &*binding };
    if !binding.initialized || binding.import_ref.is_some() || matches!(binding.value, Value::Empty)
    {
        return std::ptr::null();
    }
    &binding.value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::new_scope;

    #[test]
    fn exact_paths_have_weak_owners_and_reject_unsupported_names() {
        let holder = new_scope(None);
        holder
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(7.0), true, true));
        let reader = new_scope(Some(holder.clone()));
        let owners = Rc::strong_count(&holder);
        let guard = prepare(&reader, "x").expect("supported path");
        assert_eq!(guard.scopes.len(), 2);
        assert_eq!(Rc::strong_count(&holder), owners);
        assert!(prepare(&reader, "missing").is_none());
        let weak = Rc::downgrade(&reader);
        drop(reader);
        assert!(weak.upgrade().is_none());
        assert_eq!(guard.scopes[0].identity.as_ptr(), weak.as_ptr());
    }

    #[test]
    fn checked_binding_observes_live_tdz_import_empty_and_values() {
        let mut binding = Binding::data(Value::Num(7.0), true, true);
        assert_eq!(
            unsafe { checked_binding(&binding) },
            &binding.value as *const Value
        );
        binding.initialized = false;
        assert!(unsafe { checked_binding(&binding) }.is_null());
        binding.initialized = true;
        binding.import_ref = Some((new_scope(None), "x".into()));
        assert!(unsafe { checked_binding(&binding) }.is_null());
        binding.import_ref = None;
        binding.value = Value::Empty;
        assert!(unsafe { checked_binding(&binding) }.is_null());
        binding.value = Value::Num(9.0);
        assert_eq!(
            unsafe { checked_binding(&binding) },
            &binding.value as *const Value
        );
    }

    fn emitted_probe(guard: &Guard) -> crate::jit::JitCode {
        use crate::jit::{sys, JitCode};
        let mut a = Asm::new();
        a.stp_pre(29, 30, -16);
        let fail = a.new_label();
        let done = a.new_label();
        emit(&mut a, guard, 0, fail);
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
        JitCode {
            mem,
            len,
            pc_offsets: Vec::new(),
            max_stack: 0,
            needs_global: false,
        }
    }

    #[test]
    fn emitted_path_checks_live_ancestors_before_binding_metadata() {
        let holder = new_scope(None);
        holder
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(7.0), true, true));
        let reader = new_scope(Some(holder.clone()));
        reader
            .borrow_mut()
            .vars
            .insert("unrelated", Binding::data(Value::Undefined, true, true));
        let guard = prepare(&reader, "x").unwrap();
        let code = emitted_probe(&guard);
        let run: unsafe extern "C" fn(*const RefCell<Scope>) -> *const Value =
            unsafe { std::mem::transmute(code.mem_ptr()) };
        let probe = || unsafe { run(Rc::as_ptr(&reader)) };
        assert!(!probe().is_null());
        let shared = holder.borrow();
        assert!(!probe().is_null());
        drop(shared);
        let exclusive = holder.borrow_mut();
        assert!(probe().is_null());
        drop(exclusive);
        holder.borrow_mut().vars.get_mut("x").unwrap().initialized = false;
        assert!(probe().is_null());
        holder.borrow_mut().vars.get_mut("x").unwrap().initialized = true;
        holder.borrow_mut().vars.get_mut("x").unwrap().import_ref =
            Some((new_scope(None), "y".into()));
        assert!(probe().is_null());
        holder.borrow_mut().vars.get_mut("x").unwrap().import_ref = None;
        assert!(!probe().is_null());
        reader
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(13.0), true, true));
        assert!(probe().is_null());
        // Simulate full u32 wrap without billions of mutations. The owner-derived offset
        // addresses an initialized Cell<u32>, and no scope borrow is active during writes.
        let restore_generations = || {
            for expected in &guard.scopes {
                let owner = expected.identity.upgrade().unwrap();
                let generation = unsafe {
                    &*(Rc::as_ptr(&owner)
                        .cast::<u8>()
                        .add(layout::layout().generation)
                        .cast::<std::cell::Cell<u32>>())
                };
                generation.set(expected.generation);
            }
        };
        restore_generations();
        assert!(probe().is_null()); // revived token cannot hide an intermediate shadow
        reader.borrow_mut().vars.remove("x");
        holder.borrow_mut().vars.clear();
        holder
            .borrow_mut()
            .vars
            .insert("replacement", Binding::data(Value::Num(99.0), true, true));
        restore_generations();
        assert!(probe().is_null()); // stale holder pointer must not be dereferenced
        holder
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(23.0), true, true));
        restore_generations();
        let fresh = probe();
        assert!(!fresh.is_null());
        assert!(matches!(unsafe { &*fresh }, Value::Num(23.0)));
        holder.borrow_mut().vars.clear();
        assert!(probe().is_null()); // stale cached binding is never followed
    }
}

mod rebind;
pub(super) use rebind::{commit_rebind, prepare_rebind};
