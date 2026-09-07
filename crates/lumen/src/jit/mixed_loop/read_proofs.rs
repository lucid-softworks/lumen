//! Invocation-local, receiver-identity-checked data-entry proofs.
//!
//! The planner admits only helper-free execution and existing Number-to-Number
//! property writes. Those writes cannot resize entry storage, change descriptors
//! or prototypes, or sever an object edge. Physical VM owners retain every
//! borrowed receiver/holder until exit publication. Any extension of the effect
//! whitelist must preserve this contract or disable these proofs.
use super::{plan::Plan, probes};
use crate::{
    bytecode::{Chunk, Op},
    jit::{asm::Asm, C_NE},
    value::JitLayout,
};

/// Separate from opcode support: new native effects must explicitly opt in to
/// the stronger storage/descriptor lifetime contract required by memoization.
pub(super) fn preserves_entries(op: Op) -> bool {
    matches!(
        op,
        Op::Const(_)
            | Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::LoadThis
            | Op::LoadName(..)
            | Op::GetProp(..)
            | Op::GetPropThis(..)
            | Op::GetPropLocal(..)
            | Op::GetMethod(..)
            | Op::GetElem
            | Op::GetElemLocal(_)
            // These emitters guard ordinary existing Number -> Number only.
            | Op::SetProp(..)
            | Op::SetPropDrop(..)
            | Op::SetPropThisDrop(..)
            | Op::SetPropLocalDrop(..)
            | Op::UpdateLocal(..)
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Neg
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::EqEq
            | Op::StrictEq
            | Op::NotEq
            | Op::StrictNotEq
            | Op::Undef
            | Op::Pop
            | Op::Dup
            | Op::Dup2
            | Op::InlineGuard(..)
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
    )
}

pub(super) fn initialize(a: &mut Asm, plan: &Plan) {
    for index in 0..plan.read_proofs.len() {
        // A stored Rc is never null. Uninitialized entry pointers are never read.
        a.str_imm(31, 23, plan.proof_offset(index));
    }
}

pub(super) fn emit(
    a: &mut Asm,
    layout: &JitLayout,
    chunk: &Chunk,
    plan: &Plan,
    (pc, op): (usize, Op),
    out: u32,
    fail: usize,
) {
    let Some(index) = plan.read_proofs.iter().position(|&site| site == pc) else {
        assert!(probes::emit_read(a, layout, chunk, op, out, fail));
        return;
    };
    let offset = plan.proof_offset(index);
    let miss = a.new_label();
    let done = a.new_label();
    a.ldr_imm(9, 23, offset);
    a.cmp_reg_x(0, 9);
    a.b_cond(C_NE, miss);
    a.ldr_imm(15, 23, offset + 8);
    // Numeric payloads may have changed through aliases since the first read.
    probes::emit_proven_entry(a, layout, op, out, fail);
    #[cfg(test)]
    super::counter(a, HITS.with(|n| n.as_ptr() as usize));
    a.b(done);
    a.bind(miss);
    assert!(probes::emit_read(a, layout, chunk, op, out, fail));
    // Both probes preserve x0 and return the live entry in x15. Commit only
    // after every guard and output decode succeeds, with receiver identity last.
    a.str_imm(15, 23, offset + 8);
    a.str_imm(0, 23, offset);
    a.bind(done);
}

#[cfg(test)]
thread_local! {
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn eval(engine: &mut Engine, source: &str) {
        match engine.eval(source, false).unwrap() {
            Completion::Value(_) => {}
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    fn check(warm: &str, change: &str, threshold: u32) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            super::HITS.with(|n| n.set(0));
            eval(
                &mut engine,
                "function assert(v){if(!v)throw new Error('read proof');}",
            );
            eval(&mut engine, warm);
            let hits = super::HITS.with(|n| n.get());
            if tier == Tier::Jit
                && std::env::var_os("LUMEN_JIT_NO_MIXED_LOOP").is_none()
                && std::env::var_os("LUMEN_JIT_NO_MIXED_READ_PROOFS").is_none()
            {
                assert!(hits > 0, "memoized entry never read: {tier:?}");
            } else {
                assert_eq!(hits, 0, "unexpected memo execution: {tier:?}");
            }
            eval(&mut engine, change);
        }
    }

    #[test]
    fn numeric_alias_writes_reload_payload_and_receiver_changes_do_not_reuse_values() {
        check(
            r#"
            function scan(a,root){var total=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;total+=root.child.value;
            }return total;}
            function aliases(o){var a=[];for(var i=0;i<40;i++)a.push(o);return a;}
            var root={child:{value:1}},a=aliases(root.child);
            assert(scan(a,root)===860&&root.child.value===41);
            "#,
            r#"
            var other={child:{value:1}},b=aliases(other.child);
            assert(scan(b,other)===860&&other.child.value===41);
            root.child.value=1;assert(scan(a,root)===860);
            var constant={child:{value:9}};
            Object.defineProperty(constant.child,'value',{writable:false});
            assert(scan(a,constant)===360&&constant.child.value===9);
            "#,
            0,
        );
    }

    #[test]
    fn effectful_exit_invalidates_descriptor_and_object_graph_proofs_before_gc() {
        check(
            r#"
            function scan(a,root){var total=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;total+=root.child.value;
            }return total;}
            var a=[];for(var i=0;i<40;i++)a.push({value:1});
            var root={child:{value:3}};assert(scan(a,root)===120);
            "#,
            r#"
            var reads=0,elementReads=0,replacement={value:7};
            Object.defineProperty(a,'10',{get:function(){
                elementReads++;assert(a[9].value===3);
                Object.defineProperty(root,'child',{get:function(){reads++;return replacement;},configurable:true});
                $262.gc();return {value:1};
            },configurable:true});
            assert(scan(a,root)===240&&reads===30&&elementReads===1);
            Object.defineProperty(root,'child',{value:{value:11},writable:true,configurable:true});
            Object.defineProperty(a,'10',{value:{value:1},writable:true,configurable:true});
            $262.gc();assert(scan(a,root)===440);
            "#,
            0,
        );
    }

    #[test]
    fn array_length_proofs_reset_after_budget_and_callback_resize() {
        check(
            r#"
            function scan(a,root){var total=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;total+=root.child.length;
            }return total;}
            var a=[];for(var i=0;i<1100;i++)a.push({value:1});
            var root={child:[1,2,3]};assert(scan(a,root)===3300);
            "#,
            r#"
            root.child.push(4,5);assert(scan(a,root)===5500);
            var calls=0;
            Object.defineProperty(a,'10',{get:function(){calls++;root.child.length=1;$262.gc();return {value:1};}});
            assert(scan(a,root)===1140&&calls===1);
            "#,
            0,
        );
    }

    #[test]
    fn warmed_method_proofs_observe_replaced_prototype_and_getter_on_reentry() {
        check(
            r#"
            var proto={read:function(){return this.value;}};
            var root=Object.create(proto);root.value=3;
            function scan(a,root){var total=0,c;for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;total+=root.read();
            }return total;}
            function invoke(a,root){return scan(a,root);}
            var a=[];for(var i=0;i<40;i++)a.push({value:1});
            for(var i=0;i<600;i++)assert(invoke(a,root)===120);
            "#,
            r#"
            proto.read=function(){return this.value+1;};$262.gc();assert(invoke(a,root)===160);
            Object.setPrototypeOf(root,{read:function(){return this.value+2;}});
            assert(invoke(a,root)===200);
            var calls=0;
            Object.defineProperty(root,'read',{get:function(){calls++;$262.gc();return function(){return 9;};}});
            assert(invoke(a,root)===360&&calls===40);
            "#,
            8,
        );
    }

    #[test]
    fn maximum_proof_frame_executes_every_cell_across_budget_reentry() {
        let fields = (0..32)
            .map(|index| format!("f{index}:{}", index + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sum = (0..32)
            .map(|index| format!("root.f{index}"))
            .collect::<Vec<_>>()
            .join("+");
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            eval(
                &mut engine,
                &format!(
                    r#"
                function assert(v){{if(!v)throw new Error('maximum proof frame');}}
                function scan(a,root){{var total=0,c;for(var i=0;i<a.length;i++){{
                    c=a[i];c.value=c.value+1;total+={sum};total+=c.value;
                }}return total;}}
                var root={{{fields}}},a=[];
                for(var i=0;i<1100;i++)a.push({{value:1}});
                assert(scan(a,root)===583000);
            "#
                ),
            );
            super::HITS.with(|n| n.set(0));
            eval(
                &mut engine,
                r#"
                assert(scan(a,root)===584100);
                for(var i=0;i<1100;i++)assert(a[i].value===3);
                $262.gc();root.f31=64;
                assert(root.f0===1&&root.f31===64&&a[1099].value===3);
            "#,
            );
            let hits = super::HITS.with(|n| n.get());
            if tier == Tier::Jit
                && std::env::var_os("LUMEN_JIT_NO_MIXED_LOOP").is_none()
                && std::env::var_os("LUMEN_JIT_NO_MIXED_READ_PROOFS").is_none()
            {
                // There are 33 eligible sites (length + 32 fields), capped at 32.
                // Each runs at most 1101 times, so 31 active cells cannot pass.
                assert!(hits > 31 * 1101, "all 32 proof cells must hit: {hits}");
            } else {
                assert_eq!(hits, 0);
            }
            eval(
                &mut engine,
                "assert(scan(a,root)===620400&&a[1099].value===4);",
            );
        }
    }
}
