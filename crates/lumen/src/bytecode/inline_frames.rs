//! Inline source locations are separate from the op stream consumed by region optimizers.
use super::{Chunk, InlineTarget};
use crate::interpreter::{InlineFrame, Interp};
use std::rc::Rc;

#[derive(Default)]
pub(super) struct Builder {
    sites: Vec<(u32, u32, bool)>,
    locations: Vec<u32>,
    current: u32,
    closures: Vec<(u32, super::inline_closure::Guard)>,
}

impl Builder {
    pub fn emit(&mut self) {
        if !self.sites.is_empty() {
            self.locations.push(self.current);
        }
    }
    pub fn enter(&mut self, target: u32, strict: bool, pc: usize) -> u32 {
        let previous = self.current;
        self.locations.resize(pc, 0);
        self.sites.push((target, previous, strict));
        self.current = self.sites.len() as u32;
        previous
    }
    pub fn leave(&mut self, previous: u32) {
        self.current = previous;
    }
    pub fn checkpoint(&self) -> (usize, usize) {
        (self.sites.len(), self.closures.len())
    }
    pub fn rollback(&mut self, (sites, closures): (usize, usize), pc: usize) {
        self.sites.truncate(sites);
        self.closures.truncate(closures);
        self.locations.truncate(if sites == 0 { 0 } else { pc });
    }
    pub(super) fn closure(&mut self, target: u32, guard: super::inline_closure::Guard) {
        self.closures.push((target, guard));
    }
    pub fn finish(self, targets: &[InlineTarget]) -> Option<Locations> {
        if self.sites.is_empty() {
            return None;
        }
        let mut states: Vec<Rc<InlineFrame>> = Vec::new();
        for (target, parent, strict) in self.sites {
            let callee_slot = self
                .closures
                .iter()
                .find(|(index, _)| *index == target)
                .map(|(_, guard)| guard.slot);
            let dynamic =
                callee_slot.is_some() || parent != 0 && states[parent as usize - 1].dynamic;
            states.push(Rc::new(InlineFrame {
                parent: (parent != 0).then(|| states[parent as usize - 1].clone()),
                callee: targets[target as usize].pin.clone(),
                strict,
                callee_slot,
                dynamic,
            }));
        }
        Some(Locations {
            states,
            locations: self.locations,
            closures: self.closures,
        })
    }
}

pub(super) struct Locations {
    states: Vec<Rc<InlineFrame>>,
    locations: Vec<u32>,
    closures: Vec<(u32, super::inline_closure::Guard)>,
}

impl Chunk {
    pub(crate) fn inline_closure_target(&self, target: u32) -> bool {
        self.inline_closure(target).is_some()
    }
    pub(super) fn inline_closure(&self, target: u32) -> Option<&super::inline_closure::Guard> {
        self.inline_frames
            .as_ref()?
            .closures
            .iter()
            .find(|(index, _)| *index == target)
            .map(|(_, guard)| guard)
    }
    pub(crate) fn has_inline_closures(&self) -> bool {
        self.inline_frames
            .as_ref()
            .is_some_and(|frames| !frames.closures.is_empty())
    }
    pub(crate) fn dynamic_inline_location(&self, pc: usize) -> bool {
        // The returned immutable chain is owned by this live chunk.
        unsafe { self.inline_location(pc).as_ref() }.is_some_and(|frame| frame.dynamic)
    }
    pub(super) fn execution_strictness(&self, interp: &Interp, pc: usize) -> bool {
        if let Some(locations) = &self.inline_frames {
            let state = locations.locations[pc];
            if state != 0 {
                return locations.states[state as usize - 1].strict;
            }
        }
        interp
            .fn_frames
            .last()
            .map_or(interp.strict, |frame| frame.strict)
    }

    pub(crate) fn has_inline_frames(&self) -> bool {
        self.inline_frames.is_some()
    }
    pub(crate) fn inline_location(&self, pc: usize) -> *const InlineFrame {
        let Some(locations) = &self.inline_frames else {
            return std::ptr::null();
        };
        let state = locations.locations[pc];
        if state == 0 {
            std::ptr::null()
        } else {
            Rc::as_ptr(&locations.states[state as usize - 1])
        }
    }
    pub(crate) fn pin_inline_callees(&self, interp: &mut Interp) {
        for target in &self.inline_targets {
            if let Some(callee) = target.pin.upgrade() {
                interp.gc_pin(&callee);
            }
        }
    }
    #[inline]
    pub(crate) fn record_inline_location(
        &self,
        interp: &mut Interp,
        pc: usize,
        slots: &[crate::value::Value],
    ) {
        if let Some(frame) = interp.fn_frames.last_mut() {
            frame.record_inline(self.inline_location(pc), |slot| {
                slots[slot as usize].clone()
            });
        }
    }
    /// # Safety
    /// `ctx` owns initialized locals in the representation identified by `slots_packed`.
    #[inline]
    pub(crate) unsafe fn record_jit_inline_location(&self, ctx: &crate::jit::JitCtx, pc: usize) {
        let interp = unsafe { &mut *ctx.interp };
        if let Some(frame) = interp.fn_frames.last_mut() {
            frame.record_inline(self.inline_location(pc), |slot| {
                assert!((slot as usize) < ctx.n_slots);
                if ctx.slots_packed {
                    unsafe {
                        &*ctx
                            .slots
                            .cast::<crate::value::PackedValue>()
                            .add(slot as usize)
                    }
                    .unpack()
                } else {
                    unsafe { &*ctx.slots.add(slot as usize) }.clone()
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::value::Callable;
    use crate::{Completion, Engine};

    #[test]
    fn specialized_loop_side_exit_reconstructs_the_inlined_getters_caller() {
        let source = [
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../v8-v7/base.js")),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../v8-v7/richards.js"
            )),
            r#"
            for(let i=0;i<110;i++) runRichards();
            const expected=TaskControlBlock.prototype.isHeldOrSuspended;
            let reads=0;
            const block=new TaskControlBlock(null,ID_IDLE,1,null,{});
            Object.defineProperty(block,'state',{get:function inspectState(){
                reads++;
                if(inspectState.caller!==expected) throw 'lost inline caller on side exit';
                return STATE_HELD;
            }});
            const scheduler=new Scheduler();
            scheduler.list=block;
            scheduler.schedule();
            if(reads!==1) throw 'wrong getter count';
            'passed'
        "#,
        ]
        .join("\n");
        check(&source);
    }

    #[test]
    fn virtual_callee_survives_gc_while_active_and_is_collectable_after_return() {
        use crate::value::{set_builtin, Value};
        use std::rc::Rc;
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        let collect = engine.interp.make_native("collect", 0, |interp, _, _| {
            interp.gc_collect();
            Ok(Value::Undefined)
        });
        set_builtin(&engine.interp.global, "collect", Value::Obj(collect));
        engine
            .eval(
                r#"
            function target(holder) {
                if(holder.drop) {holder.fn=null; globalThis.target=null; collect();}
                return new Error('trace').stack.indexOf('target')>=0;
            }
            function invoke(holder) {return holder.fn(holder);}
            function drive() {
                const holder={fn:target,drop:false};
                for(let i=0;i<500;i++) if(!invoke(holder)) throw 'warm trace';
                holder.drop=true;
                if(!invoke(holder)) throw 'collected active callee';
            }
        "#,
                false,
            )
            .unwrap();
        let target = engine
            .interp
            .global
            .borrow()
            .props
            .get("target")
            .unwrap()
            .value();
        let weak = Rc::downgrade(target.as_obj().unwrap());
        drop(target);
        match engine.eval("drive(); 'passed'", false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
        assert!(engine.interp.fn_frames.is_empty());
        engine.interp.gc_collect();
        assert!(
            weak.upgrade().is_none(),
            "inactive metadata retained its callee"
        );
    }

    fn check(source: &str) -> Engine {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        match engine.eval(source, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
        assert!(
            engine.interp.fn_frames.is_empty(),
            "leaked reflection frame"
        );
        engine
    }

    #[test]
    fn warmed_inlines_preserve_caller_identity_and_native_code_is_exercised() {
        let engine = check(
            r#"
            function target(){return target.caller===invoke;}
            function invoke(){return target();}
            function drive(){for(let i=0;i<500;i++)if(!invoke())throw 'caller';}
            drive(); 'passed'
        "#,
        );
        let global = engine.interp.global.borrow();
        let invoke = global.props.get("invoke").unwrap().value();
        let object = invoke.as_obj().unwrap().borrow();
        let Callable::User(user) = &object.call else {
            panic!("user function")
        };
        let chunk = user
            .func
            .code2
            .get()
            .and_then(Option::as_ref)
            .expect("optimized caller");
        assert!(chunk.has_inline_frames());
        #[cfg(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        assert!(
            chunk.jit.get().is_some_and(Option::is_some),
            "optimized native code"
        );
    }

    #[test]
    fn nested_inlines_unwind_before_catches_and_preserve_error_stacks() {
        check(
            r#"
            function leaf(throws) {
                if(leaf.caller!==middle) throw 'leaf caller';
                if(throws) throw new Error('expected');
                return 7;
            }
            function middle(throws) {return leaf(throws);}
            function invoke(throws) {return middle(throws);}
            function drive() {
                for(let i=0;i<500;i++) if(invoke(false)!==7) throw 'result';
                for(let i=0;i<100;i++) {
                    let caught=false;
                    try {invoke(true);} catch(e) {
                        caught=e.message==='expected' && e.stack.indexOf('leaf')>=0 && e.stack.indexOf('middle')>=0;
                    }
                    if(!caught || leaf.caller!==null || middle.caller!==null) throw 'unwind';
                    if(invoke(false)!==7) throw 'after catch';
                }
            }
            drive(); 'passed'
        "#,
        );
    }

    #[test]
    fn strict_callers_remain_censored_and_callee_lifetime_survives_replacement() {
        check(
            r#"
            function target(){return target.caller;}
            function strictCaller(){'use strict';return target();}
            function replace(holder) {
                if(holder.drop) {holder.fn=null; globalThis.replace=null;}
                return new Error('trace').stack.indexOf('replace')>=0;
            }
            function invoke(holder){return holder.fn(holder);}
            function drive(){
                const holder={fn:replace,drop:false};
                for(let i=0;i<500;i++) {
                    if(strictCaller()!==null) throw 'strict caller';
                    if(!invoke(holder)) throw 'callee lifetime';
                }
                holder.drop=true;
                if(!invoke(holder)) throw 'last callee reference';
            }
            drive(); 'passed'
        "#,
        );
    }
}
