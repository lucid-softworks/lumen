//! Publish precise borrowed operand snapshots when a guard fails after an earlier write.
use super::{
    emit, name,
    sequence::{self, Plan},
    store,
    values::{Rep, Source},
};
use crate::{
    bytecode::{Chunk, Op},
    jit::{
        asm::Asm,
        region_exit::{self, Operand},
    },
    jit_ir::Cfg,
    value::JitLayout,
};

pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    pc: usize,
    labels: (&[usize], &[usize]),
    targeted: &mut [bool],
    layout: &JitLayout,
) -> bool {
    if std::env::var_os("LUMEN_JIT_NO_WRITE_SEQUENCE").is_some() {
        return false;
    }
    let Some(plan) = sequence::build(chunk, cfg, pc) else {
        return false;
    };
    if !supported(&plan, chunk, cfg, layout) {
        return false;
    }
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region] head {pc}: numeric write sequence -> {}",
            plan.join
        );
    }
    for step in &plan.steps {
        targeted[step.pc] = true;
    }
    targeted[plan.join] = true;
    // Before the first commit, every successful read is effect-free and physical state
    // is unchanged. Only later fallible operations require a precise operand snapshot.
    let guards: Vec<_> = plan
        .steps
        .iter()
        .map(|step| (step.committed && step.can_fail(&plan.values)).then(|| a.new_label()))
        .collect();
    for (step, &guard) in plan.steps.iter().zip(&guards) {
        let fail = guard.unwrap_or(labels.1[pc]);
        for at in step.values.clone() {
            emit::value(a, &plan.values, at, chunk, layout, fail);
        }
        if let Some(write) = &step.write {
            store::emit(
                a,
                layout,
                chunk,
                plan.values.values[write.receiver].register,
                plan.values.values[write.number].register,
                write.name,
                write.cache,
                fail,
            );
            #[cfg(test)]
            record(a, COMMITS.with(|n| n.as_ptr() as usize));
        }
    }
    a.b(labels.0[plan.join]);
    for (step, &guard) in plan.steps.iter().zip(&guards) {
        let Some(fail) = guard else {
            continue;
        };
        a.bind(fail);
        #[cfg(test)]
        if step.committed {
            record(a, POST_COMMIT_EXITS.with(|n| n.as_ptr() as usize));
        }
        let snapshot = operands(&plan, &step.before);
        assert!(region_exit::emit(
            a,
            layout,
            &snapshot,
            plan.prefix,
            cfg.max_settled_stack()
        ));
        // Resume beyond write-region selection, otherwise a failed entry guard
        // could immediately re-enter this same region with unchanged failing inputs.
        a.b(labels.1[step.pc]);
    }
    true
}

fn operands(plan: &Plan, snapshot: &[usize]) -> Vec<Operand> {
    snapshot
        .iter()
        .map(|&id| {
            let value = &plan.values.values[id];
            match value.rep.expect("constrained write sequence") {
                Rep::Object => Operand::Object(value.register),
                Rep::Number => Operand::Number(value.register),
            }
        })
        .collect()
}

fn supported(plan: &Plan, chunk: &Chunk, cfg: &Cfg, layout: &JitLayout) -> bool {
    plan.steps.iter().all(|step| {
        step.write
            .as_ref()
            .is_none_or(|w| store::supported(layout, chunk.jit_name(w.name)))
            && region_exit::supported(
                layout,
                &operands(plan, &step.before),
                plan.prefix,
                cfg.max_settled_stack(),
            )
    }) && plan.values.values.iter().all(|v| match v.source {
        Source::Name(n, c) => name::supported(
            layout,
            Op::LoadName(n, c),
            match v.rep {
                Some(Rep::Object) => name::Target::Object(v.register),
                _ => name::Target::Number(v.register),
            },
        ),
        _ => true,
    })
}

#[cfg(test)]
thread_local! {
    static COMMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static POST_COMMIT_EXITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record(a: &mut Asm, ptr: usize) {
    a.mov_imm64(9, ptr as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, exits: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            super::COMMITS.with(|n| n.set(0));
            super::POST_COMMIT_EXITS.with(|n| n.set(0));
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script = format!(
                "function assert(v){{if(!v)throw new Error('write sequence');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert!(super::COMMITS.with(|n| n.get() > 0), "native commit");
                if exits {
                    assert!(
                        super::POST_COMMIT_EXITS.with(|n| n.get() > 0),
                        "native exit after commit"
                    );
                }
            }
        }
    }

    #[test]
    fn later_getters_and_setters_do_not_replay_prior_writes() {
        check(
            r#"
            function update(o,n){o.a=o.a+1;o.b=o.a+o.child.value+n;return 0;}
            var o={a:0,b:0,child:{value:2}};
            for(var i=0;i<600;i++)update(o,3);
            assert(o.a===600&&o.b===605);
            var calls=0;
            Object.defineProperty(o.child,'value',{get(){calls++;assert(o.a===601);o.a=900;$262.gc();return 4;}});
            update(o,3);
            assert(calls===1&&o.a===900&&o.b===608);
            o.child={value:2};
            Object.defineProperty(o,'b',{set(v){calls++;assert(o.a===901&&v===906);$262.gc();}});
            update(o,3);assert(calls===2&&o.a===901);
        "#,
            true,
        );
    }

    #[test]
    fn chained_assignment_materializes_pending_receiver_and_value() {
        check(
            r#"
            function chain(o,n){o.left.mark=o.right.mark=n;return n;}
            var o={left:{mark:0},right:{mark:0}};
            for(var i=0;i<600;i++)assert(chain(o,i)===i);
            assert(o.left.mark===599&&o.right.mark===599);
            var calls=0;
            Object.defineProperty(o.left,'mark',{set(v){
                calls++;assert(v===700&&o.right.mark===700);
                delete o.left;delete o.right;$262.gc();this.survived=1;
                assert(this.survived===1);
            }});
            assert(chain(o,700)===700&&calls===1);
        "#,
            true,
        );
    }

    #[test]
    fn aliases_and_global_names_are_reloaded_after_commits() {
        check(
            r#"
            function pair(a,b,n){a.value=a.value+1;b.value=a.value+n;}
            var a={value:0};
            for(var i=0;i<600;i++)pair(a,a,1);
            assert(a.value===1200);
            globalThis.sequenceValue=1;globalThis.sequenceResult=0;
            function named(g){g.sequenceValue=sequenceValue+1;g.sequenceResult=sequenceValue+1;}
            for(var i=0;i<600;i++)named(globalThis);
            assert(sequenceValue===601&&sequenceResult===602);
            function edge(o,n){o.a=n;o.b=o.a;}
            var o={a:0,b:0};
            edge(o,-0);assert(1/o.a===-Infinity&&1/o.b===-Infinity);
            edge(o,Infinity);assert(o.a===Infinity&&o.b===Infinity);
            edge(o,NaN);assert(o.a!==o.a&&o.b!==o.b);
        "#,
            false,
        );
    }

    #[test]
    fn strict_failure_preserves_the_first_commit_and_outer_owner() {
        check(
            r#"
            function change(o,n){'use strict';o.a=o.a+1;o.b=n;return 2;}
            var coercions=0;
            function prefix(){return {valueOf(){coercions++;return 5;}};}
            function wrap(o){return prefix()+change(o,4);}
            function invoke(o){return wrap(o);}
            var o={a:0,b:0};
            for(var i=0;i<600;i++)assert(invoke(o)===7);
            assert(o.a===600&&coercions===600);
            Object.defineProperty(o,'b',{writable:false});
            var threw=false;try{invoke(o);}catch(e){threw=e instanceof TypeError;}
            assert(threw&&o.a===601&&o.b===4&&coercions===600);
        "#,
            true,
        );
    }
}
