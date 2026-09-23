//! Experimental publication of validated fresh-closure identities into native call caches.
use crate::{
    bytecode::{CallIc, Chunk},
    jit::JitCode,
    value::UserCallable,
};
use std::rc::Rc;
use std::sync::OnceLock;

/// Rebuild fields consumed by native probes from the already validated live closure/code.
/// Recycled Function/Chunk/JitCode addresses do not prove cached machine-code addresses
/// or direct-call flags. The caller keeps the closure alive throughout publication.
pub(super) fn rebind(
    mut ic: CallIc,
    key: usize,
    user: &UserCallable,
    chunk: &Chunk,
    code: &JitCode,
) -> CallIc {
    ic.callee = key;
    ic.env = Rc::as_ptr(&user.env);
    ic.code_mem = code.mem_ptr();
    ic.pc_offs_ptr = code.pc_offsets_ptr();
    ic.direct = chunk.jit_direct_flags(code)
        | (((chunk.inline_attempted.get() || user.func.code2.get().is_some()) as u8) << 2);
    ic
}

pub(super) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(value) = OVERRIDE.with(|v| v.get()) {
        return value;
    }
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LUMEN_JIT_REFRESH_CLOSURE_CACHE").is_some())
}

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(
    test,
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    struct Enabled(Option<bool>);
    impl Enabled {
        fn new() -> Self {
            Self(super::OVERRIDE.with(|v| v.replace(Some(true))))
        }
    }
    impl Drop for Enabled {
        fn drop(&mut self) {
            super::OVERRIDE.with(|v| v.set(self.0));
        }
    }

    fn eval(engine: &mut Engine, source: &str) {
        match engine.eval(source, false).expect("valid source") {
            Completion::Value(_) => {}
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn repeated_fresh_calls_use_identity_cache_and_keep_live_captures() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            function inspect(){eval('');return inspect.caller;}
            function factory(seed){return function(x){
                if(inspect()!==expected)throw 'caller';
                return seed+x+this.bias;
            };}
            function invoke(box){return box.f(2);}
            var expected=factory(1),box={f:expected,bias:5};
            for(var i=0;i<500;i++)if(invoke(box)!==8)throw 'warm';
        "#,
        );
        super::super::HITS.with(|v| v.set(0));
        eval(
            &mut engine,
            r#"
            expected=factory(10);box.f=expected;
            if(invoke(box)!==17)throw 'fresh';
        "#,
        );
        assert_eq!(
            super::super::HITS.with(|v| v.get()),
            1,
            "retry not exercised"
        );
        eval(
            &mut engine,
            r#"
            for(var i=0;i<100;i++)if(invoke(box)!==17)throw 'cached capture';
        "#,
        );
        assert_eq!(super::super::HITS.with(|v| v.get()), 1, "repeated retry");
        eval(
            &mut engine,
            r#"
            expected=null;box.f=null;$262.gc();
            for(var i=0;i<50;i++){
                expected=factory(i);box.f=expected;
                for(var j=0;j<10;j++)if(invoke(box)!==i+7)throw 'replacement';
            }
        "#,
        );
        assert!(engine.interp.fn_frames.is_empty());
    }

    #[test]
    fn refreshed_native_calls_unwind_and_evaluate_arguments_once() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            function factory(seed){return function(x){if(seed<0)throw x;return seed+x;};}
            function invoke(f,x){return f(x);}
            var first=factory(1);
            for(var i=0;i<500;i++)if(invoke(first,2)!==3)throw 'warm';
            var calls=0, fresh=factory(-1);
            function argument(){calls++;return 19;}
            for(var i=0;i<100;i++){
                var caught=false;
                try{invoke(fresh,argument());}catch(e){caught=e===19;}
                if(!caught)throw 'wrong throw';
            }
            if(calls!==100)throw 'argument replay';
        "#,
        );
        assert!(engine.interp.fn_frames.is_empty());
    }
}
