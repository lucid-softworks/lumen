//! Numeric loop selection that leaves every unmentioned owner in its canonical slot.
use super::{asm::Asm, emit_loop_chain, numeric_cfg, plan_loop};
use crate::{bytecode::Chunk, jit_ir::Cfg, value::JitLayout};

/// Both planners reject calls, inline guards, slot resets and unsupported effects.
/// Numeric locals are tag-guarded before publication; object receivers remain owned by
/// their original slots. Thus an already-active inline callee stays rooted on entry,
/// every side exit and every bounded backedge, without adding a new frame publication.
/// Observable fallback helpers record the inline location after locals are materialized.
pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    layout: &JitLayout,
    fast: u32,
    head: usize,
    (labels, targeted): (&[usize], &mut [bool]),
) {
    if numeric_cfg::try_emit(a, chunk, cfg, layout, head, labels, targeted) {
        return;
    }
    if let Some(plan) = plan_loop(chunk, chunk.jit_ops(), head, targeted, layout, fast, cfg) {
        let plain = emit_loop_chain(a, layout, &plan, labels);
        a.bind(plain);
        // Side exits resume at individual ops; no following fusion may swallow them.
        for flag in &mut targeted[head + 1..=plan.jump_pc] {
            *flag = true;
        }
    }
}

#[cfg(test)]
thread_local! {
    static ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn record_entry(a: &mut Asm) {
    // Preserve scratch values as well as flags: either planner can retain entry state.
    a.stp_pre(9, 10, -16);
    a.mov_imm64(9, ENTRIES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
    a.ldp_post(9, 10, 16);
}

#[cfg(test)]
mod tests {
    use crate::{
        bytecode::Tier,
        value::{set_builtin, Value},
        Completion, Engine,
    };

    fn eval(engine: &mut Engine, source: &str) {
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
        assert!(engine.interp.fn_frames.is_empty());
    }

    fn check(body: &str, expected: i32) {
        crate::bytecode::inline_closure::test_with_enabled(|| {
            let mut engine = Engine::new();
            engine.set_tier(Tier::Jit);
            engine.set_tier_threshold(0);
            let collect = engine.interp.make_native("collect", 0, |interp, _, _| {
                assert!(
                    interp.fn_frames.iter().any(|frame| !frame.inline.is_null()),
                    "missing active inline during region side exit"
                );
                assert!(
                    super::ENTRIES.with(|n| n.get()) > 0,
                    "getter did not follow a native loop entry"
                );
                interp.gc_collect();
                Ok(Value::Undefined)
            });
            set_builtin(&engine.interp.global, "collect", Value::Obj(collect));
            eval(
                &mut engine,
                &format!(
                    r#"
                function make(seed) {{
                    function leaf(a,n) {{var sum=seed; for(var i=0;i<n;i++) {{{body}}} return sum;}}
                    function invoke(a,n) {{return leaf(a,n);}}
                    return {{leaf:leaf,invoke:invoke}};
                }}
                var first=make(20);
                function warm() {{for(var j=0;j<500;j++) if(first.invoke([1,2,3,4],4)!=={expected}) throw 'warm';}}
                warm(); 'passed'
            "#
                ),
            );
            super::ENTRIES.with(|n| n.set(0));
            eval(
                &mut engine,
                &format!(
                    r#"
                var next=make(20);
                if(next.invoke([1,2,3,4],4)!=={expected}) throw 'fresh';
                'passed'
            "#
                ),
            );
            assert!(
                super::ENTRIES.with(|n| n.get()) > 0,
                "fresh inline loop never entered native region"
            );
            let bounded_result = if expected == 30 { 2068 } else { -2024 };
            eval(&mut engine, &format!("if(next.invoke(Array(2048).fill(1),2048)!=={bounded_result}) throw 'bounded continuation'; 'passed'"));
            side_exit(&mut engine, expected);
        });
    }

    fn side_exit(engine: &mut Engine, expected: i32) {
        super::ENTRIES.with(|n| n.set(0));
        eval(
            engine,
            &format!(
                r#"
            var reads=0, fail=false, a=[1,2];
            var proto=Object.create(Array.prototype); proto[3]=4;
            Object.defineProperty(proto,'2',{{get:function inspect(){{
                reads++; collect();
                if(inspect.caller!==next.leaf) throw 'lost live inline owner';
                if(fail) throw new Error('expected');
                return 3;
            }}}});
            Object.setPrototypeOf(a,proto);
            if(next.invoke(a,4)!=={expected} || reads!==1) throw 'side exit';
            fail=true; var caught=false;
            try {{next.invoke(a,4);}} catch(e) {{caught=e.message==='expected';}}
            if(!caught || next.leaf.caller!==null) throw 'unwind';
            if(next.invoke([1,2,3,4],4)!=={expected}) throw 'after throw';
            'passed'
        "#
            ),
        );
        assert!(super::ENTRIES.with(|n| n.get()) > 0);
    }

    #[test]
    fn linear_loops_in_fresh_inlines_keep_owners_across_side_exits() {
        check("sum+=a[i];", 30);
    }

    #[test]
    fn branching_loops_in_fresh_inlines_keep_owners_across_side_exits() {
        check("if(i<2)sum+=a[i];else sum-=a[i];", 16);
    }
}
