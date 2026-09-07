//! Borrowed shadow-state loops with precise publication after numeric heap effects.
mod compare_branch;
mod control;
#[cfg(test)]
mod control_tests;
mod emit;
mod numeric;
mod operations;
mod plan;
mod probes;
#[cfg(test)]
mod regression_tests;
mod shadow;
mod stats;
use crate::{
    bytecode::{Chunk, Op},
    jit::{
        asm::Asm,
        guarded_write_region::{name, store},
    },
    jit_ir::Cfg,
    value::JitLayout,
};

pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    pc: usize,
    baseline: &[usize],
    targeted: &mut [bool],
    layout: &JitLayout,
) -> bool {
    if crate::jit::PACKED_LOCAL_SLOTS
        || !crate::jit::get_method_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
        || std::env::var_os("LUMEN_JIT_NO_MIXED_LOOP").is_some()
    {
        return false;
    }
    let Some(plan) = plan::build(chunk, cfg, pc) else {
        return false;
    };
    if !plan.pcs.iter().all(|&pc| match chunk.jit_ops()[pc] {
        op @ (Op::GetProp(..)
        | Op::GetPropLocal(..)
        | Op::GetPropThis(..)
        | Op::GetMethod(..)
        | Op::GetElem
        | Op::GetElemLocal(_)) => probes::supported(layout, op),
        Op::SetProp(n, _)
        | Op::SetPropDrop(n, _)
        | Op::SetPropThisDrop(n, _)
        | Op::SetPropLocalDrop(_, n, _) => store::supported(layout, chunk.jit_name(n)),
        op @ Op::LoadName(..) => name::supported(layout, op, name::Target::Number(16)),
        _ => true,
    }) {
        return false;
    }
    for &at in plan.pcs.iter().chain(&plan.exits) {
        targeted[at] = true;
    }
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region] head {pc}: mixed shadow loop ({} ops, {} locals)",
            plan.pcs.len(),
            plan.slots
        );
    }
    emit::emit(a, chunk, cfg, &plan, layout, baseline);
    true
}

#[cfg(test)]
thread_local! {
    static ENTRIES:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
    static ITERATIONS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
    static POST_WRITE_EXITS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
}
#[cfg(test)]
fn counter(a: &mut Asm, ptr: usize) {
    a.mov_imm64(9, ptr as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}
#[cfg(test)]
fn record_entry(a: &mut Asm) {
    counter(a, ENTRIES.with(|n| n.as_ptr() as usize));
}
#[cfg(test)]
fn record_iteration(a: &mut Asm) {
    counter(a, ITERATIONS.with(|n| n.as_ptr() as usize));
}
#[cfg(test)]
fn record_exit(a: &mut Asm, flag: u32) {
    let done = a.new_label();
    a.ldr_imm(9, 23, flag);
    a.cbz(9, true, done);
    counter(a, POST_WRITE_EXITS.with(|n| n.as_ptr() as usize));
    a.bind(done);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};
    fn check(source: &str, after_write: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            super::ENTRIES.with(|n| n.set(0));
            super::ITERATIONS.with(|n| n.set(0));
            super::POST_WRITE_EXITS.with(|n| n.set(0));
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script = format!(
                "function assert(v){{if(!v)throw new Error('mixed loop');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                let entries = super::ENTRIES.with(|n| n.get());
                let iterations = super::ITERATIONS.with(|n| n.get());
                assert!(entries > 0, "no native entry");
                assert!(iterations>entries*2,"insufficient native loop coverage: {iterations} iterations / {entries} entries");
                if after_write {
                    assert!(
                        super::POST_WRITE_EXITS.with(|n| n.get() > 0),
                        "no exit after a native write"
                    );
                }
            }
        }
    }

    #[test]
    fn changing_object_locals_numeric_updates_and_budget_exits() {
        check(
            r#"
            function scan(a,n){var sum=0,c;for(var i=0;i<n;i++){c=a[i];c.value=c.value+1;sum+=c.value;}return sum;}
            var a=[];for(var k=0;k<3000;k++)a.push({value:k});
            assert(scan(a,3000)===4501500);assert(a[0].value===1&&a[2999].value===3000);
            assert(scan(a,3000)===4504500);
        "#,
            true,
        );
    }

    #[test]
    fn getter_after_prior_iterations_preserves_local_and_heap_state() {
        check(
            r#"
            function scan(a,n){var sum=0,c;for(var i=0;i<n;i++){c=a[i];c.value=c.value+1;sum+=c.value;}return sum;}
            var a=[];for(var k=0;k<20;k++)a.push({value:1});
            var calls=0;Object.defineProperty(a,'10',{get(){calls++;assert(a[0].value===2&&a[9].value===2);$262.gc();return {value:3};}});
            assert(scan(a,20)===42&&calls===1);
            assert(a[0].value===2&&a[19].value===2);
        "#,
            true,
        );
    }

    #[test]
    fn inlined_polymorphic_methods_keep_multiple_native_iterations() {
        check(
            r#"
            function A(){this.value=1;}function B(){this.value=2;}
            A.prototype.run=function(){this.value=this.value+1;};
            B.prototype.run=function(){this.value=this.value+2;};
            function execute(a){for(var i=0;i<a.length;i++){var c=a[i];c.run();}}
            function invoke(a){execute(a);}
            var a=[];for(var i=0;i<40;i++)a.push(i%2?new A():new B());
            for(var j=0;j<600;j++)invoke(a);
            assert(a[0].value===1202&&a[1].value===601);
        "#,
            true,
        );
    }
    #[test]
    fn html_dda_conditions_keep_checked_truthiness() {
        check(
            r#"
            function scan(a,condition){for(var i=0;i<a.length;i++){var c=a[i];if(condition)c.value=c.value+1;else c.value=c.value+2;}}
            var a=[];for(var i=0;i<128;i++)a.push({value:0});
            scan(a,true);assert(a[0].value===1&&a[127].value===1);
            var b=[{value:0},{value:0},{value:0},{value:0}];
            scan(b,$262.IsHTMLDDA);assert(b[0].value===2&&b[3].value===2);
        "#,
            true,
        );
    }
}
