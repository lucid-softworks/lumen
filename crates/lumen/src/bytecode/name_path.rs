//! Hash-free lexical resolution through guarded scope paths, including fresh activations.
use super::Chunk;
use crate::interpreter::{Binding, BindingLayout, Env, Interp, Scope};
use crate::value::{Exotic, Gc, Object, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

pub(crate) mod jit;

const MAX_DEPTH: usize = 8;

enum ScopeGuard {
    Layout(Weak<BindingLayout>),
    Exact {
        scope: Weak<RefCell<Scope>>,
        generation: u32,
    },
}

struct ExactGuard {
    scope: Weak<RefCell<Scope>>,
    generation: u32,
}

struct ExactPath {
    guards: Box<[ExactGuard]>,
    binding: *const Binding,
}

impl ScopeGuard {
    fn new(env: &Env, scope: &Scope) -> Self {
        match scope.vars.template_layout() {
            Some(layout) => Self::Layout(Rc::downgrade(layout)),
            None => Self::Exact {
                scope: Rc::downgrade(env),
                generation: scope.vars.generation(),
            },
        }
    }

    fn matches(&self, pointer: *const RefCell<Scope>, scope: &Scope) -> bool {
        if scope.with_obj.is_some() {
            return false;
        }
        match self {
            Self::Layout(expected) => scope
                .vars
                .template_layout()
                .is_some_and(|layout| Rc::as_ptr(layout) == expected.as_ptr()),
            Self::Exact {
                scope: expected,
                generation,
            } => pointer == expected.as_ptr() && scope.vars.generation() == *generation,
        }
    }
}

enum Holder {
    /// The final exact-scope guard proves this entry has not moved.
    Binding(*const Binding),
    /// The final layout guard proves the index names the same binding in a fresh activation.
    Slot(usize),
    Global {
        object: Weak<RefCell<Object>>,
        shape: u32,
        slot: usize,
    },
}

struct GuardedPath {
    guards: Vec<ScopeGuard>,
    holder: Holder,
}

pub(super) struct NamePath(PathKind);

enum PathKind {
    Exact(ExactPath),
    Guarded(GuardedPath),
}

impl ExactPath {
    fn read(&self, env: &Env) -> Option<Value> {
        if self
            .guards
            .first()
            .is_none_or(|guard| Rc::as_ptr(env) != guard.scope.as_ptr())
        {
            return None;
        }
        // Parent links are immutable while a live child owns the chain. Borrowing the cached
        // exact scopes directly avoids rebuilding that chain's raw pointers on every hit; the
        // weak owners keep every allocation address ABA-safe, and live generations still reject
        // any insertion/removal that could change resolution.
        for guard in &self.guards {
            // SAFETY: matching the first guard proves the original child is live. Every later
            // guard is one of its immutable ancestors, so the child chain keeps that allocation
            // alive; each cached Weak additionally prevents address reuse between invocations.
            let scope = unsafe { &*guard.scope.as_ptr() }.borrow();
            if scope.with_obj.is_some() || scope.vars.generation() != guard.generation {
                return None;
            }
        }
        let binding = unsafe { &*self.binding };
        let value =
            (binding.initialized && binding.import_ref.is_none()).then(|| binding.value.clone());
        #[cfg(test)]
        if value.is_some() {
            EXACT_HITS.with(|hits| hits.set(hits.get() + 1));
        }
        value
    }
}

#[cfg(test)]
thread_local! {
    static EXACT_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl GuardedPath {
    fn read(&self, interp: &Interp, env: &Env) -> Option<Value> {
        // The caller's strong env handle owns the entire parent chain. No JS, GC or scope
        // mutation occurs during this walk, so borrowed pointers avoid per-hop Rc churn.
        let mut pointer = Rc::as_ptr(env);
        for (index, guard) in self.guards.iter().enumerate() {
            let scope = unsafe { &*pointer }.borrow();
            if !guard.matches(pointer, &scope) {
                return None;
            }
            if index + 1 == self.guards.len() {
                return self.read_holder(interp, pointer, &scope);
            }
            pointer = Rc::as_ptr(scope.parent.as_ref()?);
        }
        None
    }

    fn read_holder(
        &self,
        interp: &Interp,
        scope_pointer: *const RefCell<Scope>,
        scope: &Scope,
    ) -> Option<Value> {
        let binding = match &self.holder {
            Holder::Binding(pointer) => unsafe { &**pointer },
            Holder::Slot(slot) => scope.vars.template_binding(*slot)?,
            Holder::Global {
                object,
                shape,
                slot,
            } => {
                if scope_pointer != Rc::as_ptr(&interp.global_env)
                    || object.as_ptr() != Rc::as_ptr(&interp.global)
                {
                    return None;
                }
                return global_value(interp, &interp.global, *shape, *slot);
            }
        };
        (binding.initialized && binding.import_ref.is_none()).then(|| binding.value.clone())
    }
}

impl NamePath {
    fn read(&self, interp: &Interp, env: &Env) -> Option<Value> {
        match self {
            Self(PathKind::Exact(path)) => path.read(env),
            Self(PathKind::Guarded(path)) => path.read(interp, env),
        }
    }

    fn build(interp: &Interp, env: &Env, name: &str) -> Option<(Self, Value)> {
        let mut guards = Vec::new();
        let mut current = env.clone();
        for depth in 0..MAX_DEPTH {
            let scope = current.borrow();
            if scope.with_obj.is_some() {
                return None;
            }
            guards.push(ScopeGuard::new(&current, &scope));
            if let Some(binding) = scope.vars.get(name) {
                // Direct bindings already have a native NameIc path. This is the fallback for
                // deeper resolutions, including depth one when the reader has no activation.
                if depth == 0 || !binding.initialized || binding.import_ref.is_some() {
                    return None;
                }
                let holder = match scope.vars.template_layout() {
                    Some(layout) => Holder::Slot(layout.slot(name)?),
                    None => Holder::Binding(binding as *const Binding),
                };
                let value = binding.value.clone();
                if let Holder::Binding(binding) = holder {
                    if guards
                        .iter()
                        .all(|guard| matches!(guard, ScopeGuard::Exact { .. }))
                    {
                        let guards = guards
                            .into_iter()
                            .map(|guard| match guard {
                                ScopeGuard::Exact { scope, generation } => {
                                    ExactGuard { scope, generation }
                                }
                                ScopeGuard::Layout(_) => unreachable!("checked exact guards"),
                            })
                            .collect();
                        return Some((Self(PathKind::Exact(ExactPath { guards, binding })), value));
                    }
                }
                return Some((
                    Self(PathKind::Guarded(GuardedPath { guards, holder })),
                    value,
                ));
            }
            if Rc::ptr_eq(&current, &interp.global_env) {
                if depth == 0 {
                    return None;
                }
                let global = interp.global.borrow();
                let slot = global.props.slot_of(name)?;
                let shape = global.props.shape();
                drop(global);
                let value = global_value(interp, &interp.global, shape, slot)?;
                let holder = Holder::Global {
                    object: Rc::downgrade(&interp.global),
                    shape,
                    slot,
                };
                return Some((
                    Self(PathKind::Guarded(GuardedPath { guards, holder })),
                    value,
                ));
            }
            let parent = scope.parent.clone()?;
            drop(scope);
            current = parent;
        }
        None
    }
}

fn global_value(interp: &Interp, object: &Gc, shape: u32, slot: usize) -> Option<Value> {
    if !interp.ordinary_get_ptr(Rc::as_ptr(object) as usize) {
        return None;
    }
    let object = object.borrow();
    if !matches!(object.exotic, Exotic::None) || object.props.shape() != shape {
        return None;
    }
    let (_, property) = object.props.entry_at(slot)?;
    (!property.accessor()).then(|| property.value())
}

impl Chunk {
    pub(super) fn name_path_hit(&self, interp: &Interp, env: &Env, cache: u32) -> Option<Value> {
        self.name_paths[cache as usize]
            .borrow()
            .as_ref()?
            .read(interp, env)
    }

    pub(super) fn name_path_fill(
        &self,
        interp: &Interp,
        env: &Env,
        name: u32,
        cache: u32,
    ) -> Option<Value> {
        let (path, value) = NamePath::build(interp, env, &self.names[name as usize])?;
        *self.name_paths[cache as usize].borrow_mut() = Some(path);
        self.record_name_number(cache as usize, &value);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{NamePath, PathKind};
    use crate::interpreter::{
        new_scope, new_var_scope_with_bindings, Binding, BindingLayout, VarMap,
    };
    use crate::value::Value;
    use crate::Engine;
    use std::rc::Rc;

    #[test]
    fn exact_paths_read_live_bindings_and_reject_chain_changes() {
        let engine = Engine::new();
        let parent = new_scope(Some(engine.interp.global_env.clone()));
        parent
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(7.0), true, true));
        let middle = new_scope(Some(parent.clone()));
        let child = new_scope(Some(middle.clone()));
        let (path, value) = NamePath::build(&engine.interp, &child, "x").unwrap();
        assert!(matches!(path, NamePath(PathKind::Exact(_))));
        assert!(matches!(value, Value::Num(7.0)));
        parent.borrow_mut().vars.get_mut("x").unwrap().value = Value::Num(9.0);
        assert!(matches!(
            path.read(&engine.interp, &child),
            Some(Value::Num(9.0))
        ));

        let other = new_scope(Some(middle.clone()));
        assert!(path.read(&engine.interp, &other).is_none());
        parent.borrow_mut().vars.get_mut("x").unwrap().initialized = false;
        assert!(path.read(&engine.interp, &child).is_none());
        parent.borrow_mut().vars.get_mut("x").unwrap().initialized = true;
        parent.borrow_mut().vars.get_mut("x").unwrap().import_ref =
            Some((engine.interp.global_env.clone(), "x".to_string()));
        assert!(path.read(&engine.interp, &child).is_none());
        parent.borrow_mut().vars.get_mut("x").unwrap().import_ref = None;
        middle.borrow_mut().with_obj = Some(Value::Undefined);
        assert!(path.read(&engine.interp, &child).is_none());
        middle.borrow_mut().with_obj = None;
        assert!(matches!(
            path.read(&engine.interp, &child),
            Some(Value::Num(9.0))
        ));
        middle
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(11.0), true, true));
        assert!(path.read(&engine.interp, &child).is_none());
    }

    #[test]
    fn paths_follow_live_values_and_reject_shadowing_and_different_ancestors() {
        let engine = Engine::new();
        let parent = new_scope(Some(engine.interp.global_env.clone()));
        parent
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(7.0), true, true));
        let layout = BindingLayout::new([Rc::from("local")]);
        let first =
            new_var_scope_with_bindings(Some(parent.clone()), VarMap::from_layout(layout.clone()));
        let (path, value) = NamePath::build(&engine.interp, &first, "x").unwrap();
        assert!(matches!(value, Value::Num(7.0)));
        let second =
            new_var_scope_with_bindings(Some(parent.clone()), VarMap::from_layout(layout.clone()));
        parent.borrow_mut().vars.get_mut("x").unwrap().value = Value::Num(9.0);
        assert!(matches!(
            path.read(&engine.interp, &second),
            Some(Value::Num(9.0))
        ));
        let other = new_scope(Some(engine.interp.global_env.clone()));
        other
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(11.0), true, true));
        let third = new_var_scope_with_bindings(Some(other), VarMap::from_layout(layout));
        assert!(path.read(&engine.interp, &third).is_none());
        second
            .borrow_mut()
            .vars
            .insert("x", Binding::data(Value::Num(13.0), true, true));
        assert!(path.read(&engine.interp, &second).is_none());
        parent.borrow_mut().vars.remove("x");
        assert!(path.read(&engine.interp, &first).is_none());
    }

    #[test]
    fn layout_holders_read_the_current_activation_and_tdz_flag() {
        let engine = Engine::new();
        let layout = BindingLayout::new([Rc::from("x")]);
        let parent = new_var_scope_with_bindings(
            Some(engine.interp.global_env.clone()),
            VarMap::from_layout(layout.clone()),
        );
        let child_layout = BindingLayout::new([Rc::from("local")]);
        let child =
            new_var_scope_with_bindings(Some(parent), VarMap::from_layout(child_layout.clone()));
        let (path, _) = NamePath::build(&engine.interp, &child, "x").unwrap();
        let fresh = new_var_scope_with_bindings(
            Some(engine.interp.global_env.clone()),
            VarMap::from_layout(layout.clone()),
        );
        fresh
            .borrow_mut()
            .vars
            .layout_binding_mut(&layout, 0)
            .unwrap()
            .value = Value::Num(17.0);
        let reader =
            new_var_scope_with_bindings(Some(fresh.clone()), VarMap::from_layout(child_layout));
        assert!(matches!(
            path.read(&engine.interp, &reader),
            Some(Value::Num(17.0))
        ));
        fresh
            .borrow_mut()
            .vars
            .layout_binding_mut(&layout, 0)
            .unwrap()
            .initialized = false;
        assert!(path.read(&engine.interp, &reader).is_none());
        assert!(NamePath::build(&engine.interp, &reader, "x").is_none());
    }

    #[test]
    fn global_paths_reject_accessors_deletion_and_other_realms() {
        let mut engine = Engine::new();
        engine.eval("globalThis.pathValue=3", false).unwrap();
        let env = new_scope(Some(engine.interp.global_env.clone()));
        let (path, _) = NamePath::build(&engine.interp, &env, "pathValue").unwrap();
        engine.eval("globalThis.pathValue=5", false).unwrap();
        assert!(matches!(
            path.read(&engine.interp, &env),
            Some(Value::Num(5.0))
        ));
        let other = Engine::new();
        assert!(path.read(&other.interp, &env).is_none());
        engine.eval("globalThis.getterCalls=0; Object.defineProperty(globalThis,'pathValue',{configurable:true,get(){getterCalls++;return 7}})", false).unwrap();
        assert!(path.read(&engine.interp, &env).is_none());
        assert!(NamePath::build(&engine.interp, &env, "pathValue").is_none());
        assert!(
            matches!(engine.eval("getterCalls", false).unwrap(), crate::Completion::Value(v) if v == "0")
        );
        engine.eval("delete globalThis.pathValue", false).unwrap();
        assert!(path.read(&engine.interp, &env).is_none());
    }
}
