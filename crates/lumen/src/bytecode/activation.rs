//! Shared lexical layouts and per-call binding values for compiled closures.
use super::{CapInit, Chunk};
use crate::interpreter::{Binding, BindingLayout, Env, Interp, VarMap};
use crate::value::Value;
use std::rc::Rc;

pub(super) struct ActivationLayout {
    bindings: Rc<BindingLayout>,
    init_slots: Vec<usize>,
    name_slots: Vec<Option<usize>>,
    this_slot: Option<usize>,
}

fn init_name(init: &CapInit) -> &Rc<str> {
    match init {
        CapInit::Param(_, name)
        | CapInit::Var(name)
        | CapInit::Fn(_, name)
        | CapInit::Lexical(name, _) => name,
    }
}

impl ActivationLayout {
    fn base(&self, env: &Env) -> *mut Binding {
        let mut scope = env.borrow_mut();
        if scope.vars.generation() != 0 {
            return std::ptr::null_mut();
        }
        scope
            .vars
            .layout_base(&self.bindings)
            .unwrap_or(std::ptr::null_mut())
    }
    pub(super) fn new(inits: &[CapInit], env_this: bool, names: &[Rc<str>]) -> Option<Self> {
        if inits.is_empty() && !env_this {
            return None;
        }
        let bindings = BindingLayout::new(
            inits
                .iter()
                .map(|init| init_name(init).clone())
                .chain(env_this.then(|| Rc::from("this"))),
        );
        Some(Self {
            init_slots: inits
                .iter()
                .map(|init| bindings.slot(init_name(init)).unwrap())
                .collect(),
            name_slots: names.iter().map(|name| bindings.slot(name)).collect(),
            this_slot: env_this.then(|| bindings.slot("this").unwrap()),
            bindings,
        })
    }

    pub(super) fn binding_mut<'a>(
        &self,
        vars: &'a mut VarMap,
        name: usize,
    ) -> Option<&'a mut Binding> {
        vars.layout_binding_mut(&self.bindings, self.name_slots[name]?)
    }

    pub(super) fn make_env(
        &self,
        chunk: &Chunk,
        interp: &Interp,
        parent: &Env,
        this: &Value,
        args: &[Value],
    ) -> Env {
        let act = crate::interpreter::new_var_scope_with_bindings(
            Some(parent.clone()),
            VarMap::from_layout(self.bindings.clone()),
        );
        {
            let mut scope = act.borrow_mut();
            for (init, &slot) in chunk.cap_inits.iter().zip(&self.init_slots) {
                let binding = match init {
                    CapInit::Param(k, _) => Binding::data(
                        args.get(*k as usize).cloned().unwrap_or(Value::Undefined),
                        true,
                        true,
                    ),
                    // Layout slots start as initialized mutable undefined bindings. A same-name
                    // var must not overwrite an earlier parameter, matching declaration hoisting.
                    CapInit::Var(_) | CapInit::Fn(..) => continue,
                    CapInit::Lexical(_, is_const) => {
                        Binding::data(Value::Undefined, !is_const, false)
                    }
                };
                *scope.vars.layout_binding_mut(&self.bindings, slot).unwrap() = binding;
            }
            if let Some(slot) = self.this_slot {
                *scope.vars.layout_binding_mut(&self.bindings, slot).unwrap() =
                    Binding::data(this.clone(), false, true);
            }
        }
        // Hoisted closures capture the completed activation and overwrite parameter/var
        // collisions in declaration order, without retaining a per-call list of names.
        for (init, &slot) in chunk.cap_inits.iter().zip(&self.init_slots) {
            if let CapInit::Fn(index, _) = init {
                let value = interp.make_function(chunk.funcs[*index as usize].clone(), act.clone());
                *act.borrow_mut()
                    .vars
                    .layout_binding_mut(&self.bindings, slot)
                    .unwrap() = Binding::data(value, true, true);
            }
        }
        act
    }
}

impl Chunk {
    pub(crate) fn jit_capture_base(&self, env: &Env) -> *mut Binding {
        self.activation_layout
            .as_ref()
            .map_or(std::ptr::null_mut(), |layout| layout.base(env))
    }

    pub(crate) fn jit_capture_offset(&self, name: u32) -> Option<usize> {
        self.activation_layout.as_ref()?.name_slots[name as usize]?
            .checked_mul(std::mem::size_of::<Binding>())
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::interpreter::Binding;
    use crate::value::{set_builtin, Callable, Value};
    use crate::{Completion, Engine};

    #[test]
    fn captured_access_survives_structural_change_during_a_native_callback() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let mutate = engine
                .interp
                .make_native("mutate", 1, |_interp, _this, args| {
                    let object = args[0].as_obj().unwrap().borrow();
                    let Callable::User(user) = &object.call else {
                        panic!("closure")
                    };
                    let env = user.env.clone();
                    drop(object);
                    let mut scope = env.borrow_mut();
                    scope
                        .vars
                        .insert("injected", Binding::data(Value::Num(1.0), true, true));
                    for index in 0..20 {
                        scope.vars.insert(
                            format!("extra{index}"),
                            Binding::data(Value::Undefined, true, true),
                        );
                    }
                    Ok(Value::Undefined)
                });
            set_builtin(&engine.interp.global, "mutate", Value::Obj(mutate));
            let result = engine.eval(
                "function f() { let x={n:7}; const read=()=>x; mutate(read); x={n:9}; return x===read() && x.n===9; }\n\
                 function drive() { for(let i=0;i<500;i++) if(!f()) throw 'stale capture'; } drive(); 'passed'",
                false,
            ).unwrap();
            assert!(
                matches!(result, Completion::Value(ref v) if v == "passed"),
                "{tier:?}: unexpected completion"
            );
        }
    }
}
