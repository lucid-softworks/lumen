//! Function-keyed call retry. The returned IC is ephemeral and owns no extra roots.
use super::{Callable, Env, Interp};
use crate::bytecode::{CallIc, CallSite, CALL_IC_NEEDS_ENV};
use crate::value::Gc;
use std::{rc::Rc, sync::OnceLock};

fn enabled() -> bool {
    #[cfg(test)]
    if let Some(value) = OVERRIDE.with(|v| v.get()) {
        return value;
    }
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LUMEN_JIT_FRESH_CLOSURE_RETRY").is_some())
}

impl Interp {
    pub(super) fn fresh_call_ic(
        &self,
        site: &CallSite,
        o: &Gc,
        key: usize,
        genv: usize,
        epoch: u32,
    ) -> Option<CallIc> {
        self.activation_call_ic(site, o, key, genv, epoch)
            .or_else(|| {
                enabled()
                    .then(|| self.lean_call_ic(site, o, key, genv, epoch))
                    .flatten()
            })
    }

    // Preserve the existing activation retry's eligibility and observation behavior.
    fn activation_call_ic(
        &self,
        site: &CallSite,
        o: &Gc,
        key: usize,
        genv: usize,
        epoch: u32,
    ) -> Option<CallIc> {
        // Fresh-closure retry: a needs-env entry whose AST FUNCTION matches the callee
        // still applies — closures created per call share the function and chunk; only
        // the environment differs, and that comes from the live callee object. One
        // borrow + pointer compare instead of a full call_jit_fast re-walk + refill
        // per instance.
        let has_env_entry = site.entries.iter().any(|e| {
            let p = e.as_ptr();
            unsafe { (*p).direct & crate::bytecode::CALL_IC_NEEDS_ENV != 0 }
        });
        if !has_env_entry {
            return None;
        }
        let (fp, ep) = match &o.borrow().call {
            // `under_with`: see the refusal in `call_jit_fast` — this retry runs a
            // FRESH closure instance whose env was never vetted there.
            Callable::User(user) if !user.func.is_arrow && !user.env.borrow().under_with => {
                (Rc::as_ptr(&user.func), Rc::as_ptr(&user.env))
            }
            _ => return None,
        };
        let mut found = None;
        for e in &site.entries {
            let p = e.as_ptr();
            unsafe {
                if (*p).direct & crate::bytecode::CALL_IC_NEEDS_ENV != 0
                    && (*p).func == fp
                    && (*p).global_env == genv
                    && (*p).epoch == epoch
                {
                    found = Some(*p);
                    break;
                }
            }
        }
        let mut ic = found?;
        // The cached code belongs to the shared AST, but reflection and the
        // frame's lifetime proof must identify the live closure on the stack.
        ic.callee = key;
        ic.env = ep;
        Some(ic)
    }

    fn lean_call_ic(
        &self,
        site: &CallSite,
        o: &Gc,
        key: usize,
        genv: usize,
        epoch: u32,
    ) -> Option<CallIc> {
        let candidates = site.entries.iter().filter(|entry| {
            // Only inspect identity fields; no callback or cache mutation occurs while
            // these temporary views are live. Copy the complete IC only on acceptance.
            let ic = unsafe { &*entry.as_ptr() };
            ic.callee != 0
                && ic.native == 0
                && ic.direct & CALL_IC_NEEDS_ENV == 0
                && ic.epoch == epoch
                && ic.global_env == genv
                && !ic.func.is_null()
        });
        candidates.clone().next()?;
        let object = o.try_borrow().ok()?;
        let Callable::User(user) = &object.call else {
            return None;
        };
        let fp = Rc::as_ptr(&user.func);
        let mut matching = candidates
            .filter(|entry| unsafe { (*entry.as_ptr()).func == fp })
            .peekable();
        matching.peek()?;
        if !self.ordinary_get_ptr(key)
            || self.class_info.contains_key(&key)
            || !object.ic_plain.get()
        {
            return None;
        }
        if user.func.is_arrow || user.env.try_borrow().ok()?.under_with {
            return None;
        }
        if self.multi_realm() && !same_root(&user.env, &self.global_env) {
            return None;
        }
        let selected = user
            .func
            .code2
            .get()
            .or_else(|| user.func.code.get())?
            .as_ref()?;
        if !selected.jit_no_activation() {
            return None;
        }
        let code = selected.jit.get()?.as_ref()?;
        let (n_params, n_slots) = selected.jit_frame();
        let n_params = u16::try_from(n_params).ok()?;
        let n_slots = u16::try_from(n_slots).ok()?;
        let uses_this = selected.uses_this();
        for entry in matching {
            let ic = unsafe { &*entry.as_ptr() };
            if ic.chunk_raw != Rc::as_ptr(selected)
                || ic.code != Rc::as_ptr(code)
                || !std::ptr::eq(ic.chunk, selected)
                // Weak object pins do not pin old Function/code allocations. Matching
                // recycled pointers must not authorize stale frame dimensions or semantics.
                || ic.strict != user.func.is_strict
                || ic.uses_this != uses_this
                || ic.n_params != n_params
                || ic.n_slots != n_slots
            {
                continue;
            }
            let mut ic = *ic;
            ic.callee = key;
            ic.env = Rc::as_ptr(&user.env);
            #[cfg(test)]
            HITS.with(|v| v.set(v.get() + 1));
            return Some(ic);
        }
        None
    }
}

fn same_root(env: &Env, global: &Env) -> bool {
    let mut current = Rc::as_ptr(env);
    for _ in 0..8 {
        if current == Rc::as_ptr(global) {
            return true;
        }
        // The live callee owns the complete unchanged strong parent chain.
        let Ok(scope) = (unsafe { &*current }).try_borrow() else {
            return false;
        };
        let Some(parent) = &scope.parent else {
            return false;
        };
        current = Rc::as_ptr(parent);
    }
    false
}

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(
    test,
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use super::*;
    use crate::{bytecode::Tier, Completion, Engine};

    struct Enabled(Option<bool>);
    impl Enabled {
        fn new() -> Self {
            HITS.with(|v| v.set(0));
            Self(OVERRIDE.with(|v| v.replace(Some(true))))
        }
    }
    impl Drop for Enabled {
        fn drop(&mut self) {
            OVERRIDE.with(|v| v.set(self.0));
        }
    }
    fn run(engine: &mut Engine, source: &str) {
        let result = engine.eval(source, false).expect("valid source");
        assert!(
            matches!(result, Completion::Value(_)),
            "unexpected JavaScript throw"
        );
    }
    fn engine() -> Engine {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        engine
    }

    #[test]
    fn fresh_environments_keep_live_reflected_callee_and_this() {
        let _enabled = Enabled::new();
        let mut engine = engine();
        run(
            &mut engine,
            r#"
            function inspect(){eval('');return inspect.caller;}
            function factory(seed){return function(x){
                if(inspect()!==expected)throw 'caller';
                return seed+x+this.bias;
            };}
            function invoke(box,x){return box.f(x);}
            var expected=factory(1),box={f:expected,bias:5};
            for(var i=0;i<500;i++)if(invoke(box,2)!==8)throw 'warm';
            expected=null;box.f=null;$262.gc();
            for(var i=0;i<20;i++){
                expected=factory(i);box.f=expected;
                if(invoke(box,2)!==i+7)throw 'fresh env/this';
            }
        "#,
        );
        assert!(HITS.with(|v| v.get()) > 0, "no lean retry selected");
        assert!(engine.interp.fn_frames.is_empty());
    }

    #[test]
    fn fresh_retry_throw_consumes_arguments_once_and_unwinds() {
        let _enabled = Enabled::new();
        let mut engine = engine();
        run(
            &mut engine,
            r#"
            function factory(seed){return function(x){if(seed<0)throw x;return seed+x;};}
            function invoke(f,x){return f(x);}
            var first=factory(1);
            for(var i=0;i<500;i++)if(invoke(first,2)!==3)throw 'warm';
            var calls=0;
            function argument(){calls++;return 19;}
            for(var i=0;i<20;i++){
                var caught=false;
                try{invoke(factory(-1),argument());}catch(e){caught=e===19;}
                if(!caught)throw 'wrong throw';
            }
            if(calls!==20)throw 'argument replay';
        "#,
        );
        assert!(HITS.with(|v| v.get()) > 0, "no lean retry selected");
        assert!(engine.interp.fn_frames.is_empty());
    }

    #[test]
    fn fresh_calls_retain_the_normal_inline_recompile_opportunity() {
        let _enabled = Enabled::new();
        let mut engine = engine();
        run(
            &mut engine,
            r#"
            function factory(seed){return function(x){return seed+x;};}
            function invoke(f){return f(3);}
            var last=factory(1);
            if(invoke(last)!==4)throw 'initial';
            for(var i=0;i<600;i++){
                last=factory(i);
                if(invoke(last)!==i+3)throw 'tiered capture';
            }
        "#,
        );
        assert!(HITS.with(|v| v.get()) > 0, "no lean retry selected");
        let global = crate::value::Value::Obj(engine.interp.global.clone());
        let last = engine
            .interp
            .get_member(&global, "last")
            .unwrap_or_else(|_| panic!("last"));
        let crate::value::Value::Obj(last) = last else {
            panic!("function")
        };
        let object = last.borrow();
        let Callable::User(user) = &object.call else {
            panic!("user")
        };
        let base = user
            .func
            .code
            .get()
            .and_then(Option::as_ref)
            .expect("compiled");
        assert!(base.jit_runs.get() >= crate::bytecode::inline_recompile_at());
        assert!(
            base.inline_attempted.get(),
            "fresh calls never reached recompile trigger"
        );
    }
    #[test]
    fn matching_live_pointers_cannot_authorize_stale_frame_metadata() {
        let mut engine = engine();
        run(
            &mut engine,
            "var target=function(x){return this.bias+x;};target.call({bias:3},4);",
        );
        let global = crate::value::Value::Obj(engine.interp.global.clone());
        let target = engine
            .interp
            .get_member(&global, "target")
            .unwrap_or_else(|_| panic!("target"));
        let crate::value::Value::Obj(target) = target else {
            panic!("function")
        };
        let key = Rc::as_ptr(&target) as usize;
        let genv = Rc::as_ptr(&engine.interp.global_env) as usize;
        let epoch = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        let mut baseline = CallIc::EMPTY;
        {
            let object = target.borrow();
            let Callable::User(user) = &object.call else {
                panic!("user")
            };
            let selected = user
                .func
                .code2
                .get()
                .or_else(|| user.func.code.get())
                .and_then(Option::as_ref)
                .expect("compiled");
            let code = selected
                .jit
                .get()
                .and_then(Option::as_ref)
                .expect("native code");
            let (params, slots) = selected.jit_frame();
            baseline.callee = key;
            baseline.func = Rc::as_ptr(&user.func);
            baseline.env = Rc::as_ptr(&user.env);
            baseline.global_env = genv;
            baseline.epoch = epoch;
            baseline.chunk = selected;
            baseline.chunk_raw = Rc::as_ptr(selected);
            baseline.code = Rc::as_ptr(code);
            baseline.strict = user.func.is_strict;
            baseline.uses_this = selected.uses_this();
            baseline.n_params = u16::try_from(params).unwrap();
            baseline.n_slots = u16::try_from(slots).unwrap();
        }
        let site = CallSite::empty();
        site.entries[0].set(baseline);
        assert!(engine
            .interp
            .lean_call_ic(&site, &target, key, genv, epoch)
            .is_some());
        for field in 0..4 {
            let mut stale = baseline;
            match field {
                0 => stale.strict = !stale.strict,
                1 => stale.uses_this = !stale.uses_this,
                2 => stale.n_params = stale.n_params.wrapping_add(1),
                _ => stale.n_slots = stale.n_slots.wrapping_add(1),
            }
            site.entries[0].set(stale);
            assert!(
                engine
                    .interp
                    .lean_call_ic(&site, &target, key, genv, epoch)
                    .is_none(),
                "field {field}"
            );
        }
    }
}
