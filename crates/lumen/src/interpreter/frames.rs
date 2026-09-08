//! Physical frames and source-location metadata for calls spliced into optimized code.
use super::{Env, Interp};
use crate::value::{Gc, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

/// One entry of the legacy `fn.caller`/`fn.arguments` reflection stack (see `call_user`). The
/// arguments object materializes lazily: a body that never names `arguments` skips building it,
/// and `lazy` keeps what a later reflective read needs to conjure it on demand.
/// `repr(C)`: the asm call sequence (arc 3b) pushes frames from machine code — fn_ptr@0,
/// coro@8, strict@12, extra@16, inline@24, size 32 (asserted in jit.rs).
#[repr(C)]
pub struct FnFrame {
    /// `Rc::as_ptr` of the callee. No strong handle is kept: every frame is pushed while its
    /// caller holds the callee alive (the callee `Value` sits on the caller's operand stack or in
    /// the dispatch chain for the whole call — for frames owned by a parked coroutine, the
    /// worker's frozen stack; a torn-down coroutine's worker parks forever rather than unwinding,
    /// which this invariant depends on), so the rare reflective reads reconstruct one via
    /// [`FnFrame::callee`] instead of paying a refcount round-trip on every call.
    pub fn_ptr: usize,
    /// Owning coroutine body (`Interp::cur_coro`; 0 = the main driver): a worker-thread panic
    /// evicts the dead body's frames by this tag (see `ThreadCoro::resume`).
    pub coro: u32,
    pub strict: bool,
    /// The rare per-frame state (a live `arguments` object, or what a reflective `fn.arguments`
    /// read needs to conjure one). Boxed so the common frame stays compact — frames are pushed
    /// and popped on EVERY call, and the pop's copy-out and drop-check of a fat frame was a
    /// measurable slice of the call path.
    pub extra: Option<Box<FrameExtra>>,
    /// Immutable inline call chain at the most recent observable operation.
    pub(crate) inline: *const InlineFrame,
}

/// See [`FnFrame::extra`].
pub struct FrameExtra {
    pub args_obj: Value,
    pub lazy: Option<(Rc<crate::ast::Function>, Rc<[Value]>, Env)>,
}

impl Default for FrameExtra {
    fn default() -> FrameExtra {
        FrameExtra {
            args_obj: Value::Null,
            lazy: None,
        }
    }
}

impl FnFrame {
    /// A strong handle to the callee, reconstructed from `fn_ptr` (see its aliveness invariant).
    pub fn callee(&self) -> Gc {
        let p = self.fn_ptr as *const RefCell<crate::value::Object>;
        unsafe {
            Rc::increment_strong_count(p);
            Rc::from_raw(p)
        }
    }
}

/// Shared immutable state owned by the compiled chunk. Weak targets are GC-pinned at install;
/// active locations become explicit roots during collection, inactive targets remain collectable.
pub(crate) struct InlineFrame {
    pub parent: Option<Rc<InlineFrame>>,
    pub callee: Weak<RefCell<crate::value::Object>>,
    pub strict: bool,
}

pub(crate) struct ReflectedFrame {
    pub fn_ptr: usize,
    pub strict: bool,
    pub physical: Option<usize>,
}

impl ReflectedFrame {
    pub fn callee(&self) -> Gc {
        let pointer = self.fn_ptr as *const RefCell<crate::value::Object>;
        unsafe {
            Rc::increment_strong_count(pointer);
            Rc::from_raw(pointer)
        }
    }
}

impl Interp {
    pub(crate) fn reflected_frames(&self) -> Vec<ReflectedFrame> {
        let mut frames = Vec::new();
        for (index, frame) in self.fn_frames.iter().enumerate() {
            frames.push(ReflectedFrame {
                fn_ptr: frame.fn_ptr,
                strict: frame.strict,
                physical: Some(index),
            });
            let start = frames.len();
            // The physical callee/run owns its immutable Chunk until this frame is popped.
            let mut state = unsafe { frame.inline.as_ref() };
            while let Some(inline) = state {
                if inline.callee.strong_count() != 0 {
                    frames.push(ReflectedFrame {
                        fn_ptr: inline.callee.as_ptr() as usize,
                        strict: inline.strict,
                        physical: None,
                    });
                }
                state = inline.parent.as_deref();
            }
            frames[start..].reverse();
        }
        frames
    }

    pub(crate) fn inline_gc_roots(&self) -> Vec<Gc> {
        let mut roots = Vec::new();
        for frame in &self.fn_frames {
            let mut state = unsafe { frame.inline.as_ref() };
            while let Some(inline) = state {
                if let Some(callee) = inline.callee.upgrade() {
                    roots.push(callee);
                }
                state = inline.parent.as_deref();
            }
        }
        roots
    }
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn fresh_activation_closure_reflects_the_actual_cached_call_target() {
        let source = r#"
            function inspect() { eval(''); return inspect.caller; }
            function factory() {
                return function() {
                    let captured = 1;
                    function inner() { return captured; }
                    if (inner() !== 1) throw 'capture';
                    return inspect();
                };
            }
            function invoke(f) { return f(); }
            const first = factory();
            for (let i = 0; i < 500; i++) {
                if (invoke(first) !== first) throw 'warm caller';
            }
            for (let i = 0; i < 10; i++) {
                const next = factory();
                if (invoke(next) !== next) throw 'fresh caller';
            }
            'passed'
        "#;
        for tier in [Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed"),
                Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
            assert!(engine.interp.fn_frames.is_empty());
        }
    }
}
