//! Guarded acyclic branches with one non-destructive numeric heap write.
mod emit;
mod name;
mod plan;
mod store;
mod values;

use crate::{
    bytecode::Chunk,
    jit::{asm::Asm, PACKED_LOCAL_SLOTS},
    jit_ir::Cfg,
    value::JitLayout,
};

pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    pc: usize,
    labels: &[usize],
    targeted: &mut [bool],
    layout: &JitLayout,
) -> bool {
    if PACKED_LOCAL_SLOTS
        || !crate::jit::get_method_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
        || std::env::var_os("LUMEN_JIT_NO_GUARDED_WRITE_REGION").is_some()
    {
        return false;
    }
    let Some(plan) = plan::build(chunk, cfg, pc) else {
        return false;
    };
    if !emit::supported(&plan.root, chunk, layout) {
        return false;
    }
    if std::env::var_os("LUMEN_JIT_REGIONLOG").is_some() {
        eprintln!(
            "[jit-region] head {pc}: guarded numeric write -> {}",
            plan.join
        );
    }
    // Every original PC remains an independent baseline entry. No owner, stack or
    // binding changes precede the write; a guard miss resumes at the original start.
    for at in plan.pcs {
        targeted[at] = true;
    }
    targeted[plan.join] = true;
    let fail = a.new_label();
    emit::emit(
        a,
        &plan.root,
        chunk,
        layout,
        fail,
        labels[plan.join],
        plan.prefix_depth,
    );
    a.bind(fail);
    true
}

#[cfg(test)]
thread_local! {
    static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PREFIX_SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_success(a: &mut Asm, prefix_depth: usize) {
    a.mov_imm64(9, SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
    if prefix_depth != 0 {
        a.mov_imm64(9, PREFIX_SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
        a.ldr_imm(10, 9, 0);
        a.add_imm(10, 10, 1);
        a.str_imm(10, 9, 0);
    }
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn pending_owned_operand_survives_commit_and_getter_gc_fallback() {
        super::PREFIX_SUCCESSES.with(|n| n.set(0));
        check(
            r#"
            var coercions=0;
            function prefix(){return {valueOf:function(){coercions++;return 5;}};}
            function update(o){if(o.direction===1)o.dst.value=o.left.value+o.right.value;else o.dst.value=o.left.value-o.right.value;return 2;}
            function wrap(o){return prefix()+update(o);}
            function invoke(o){return wrap(o);}
            var o={direction:1,dst:{value:0},left:{value:10},right:{value:3}};
            for(var i=0;i<600;i++){assert(invoke(o)===7);assert(o.dst.value===13);}
            assert(coercions===600);
            var gets=0;
            Object.defineProperty(o.right,'value',{get(){gets++;$262.gc();return 4;}});
            assert(invoke(o)===7 && o.dst.value===14 && coercions===601 && gets===1);
        "#,
        );
        assert!(
            super::PREFIX_SUCCESSES.with(|n| n.get()) > 0,
            "pending-prefix region never committed"
        );
    }

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let source = format!(
                "function assert(v){{if(!v)throw new Error('guarded write');}} {source}; 'passed'"
            );
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier == Tier::Jit {
                assert!(
                    super::SUCCESSES.with(|n| n.get()) > 0,
                    "no guarded write executed"
                );
            }
        }
    }

    #[test]
    fn numeric_diamond_global_roots_aliases_and_number_edges() {
        check(
            r#"
            var Direction={FORWARD:1};
            function update(o){
                if(o.direction===Direction.FORWARD)
                    o.dst.value=o.src.value*o.scale.value+o.offset.value;
                else o.src.value=(o.dst.value-o.offset.value)/o.scale.value;
            }
            var o={direction:1,dst:{value:0},src:{value:5},scale:{value:2},offset:{value:3}};
            for(var i=0;i<600;i++){
                o.direction=1;update(o);assert(o.dst.value===13);
                o.direction=0;update(o);assert(o.src.value===5);
            }
            o.dst=o.src;o.direction=1;update(o);assert(o.src.value===13);
            o.direction=0;o.scale.value=0;o.offset.value=13;update(o);assert(Number.isNaN(o.src.value));
            o.dst={value:1};o.offset.value=0;update(o);assert(o.src.value===Infinity);
            o.dst.value=-0;o.scale.value=1;update(o);assert(Object.is(o.src.value,-0));
            Direction={FORWARD:0};o.src.value=4;update(o);assert(o.dst.value===4);
            o.extra=1;o.dst.extra=2;update(o);assert(o.dst.value===4);
            Object.preventExtensions(o.dst);update(o);assert(o.dst.value===4);
        "#,
        );
    }

    #[test]
    fn guards_preserve_observable_reads_writes_and_strict_failures() {
        check(
            r#"
            function update(o){if(o.direction===1)o.dst.value=o.left.value+o.right.value;else o.dst.value=o.left.value-o.right.value;}
            function strictUpdate(o){'use strict';if(o.direction===1)o.dst.value=o.left.value+o.right.value;else o.dst.value=o.left.value-o.right.value;}
            var o={direction:1,dst:{value:0},left:{value:10},right:{value:3}};
            for(var i=0;i<600;i++){update(o);assert(o.dst.value===13);}
            var reads=0,writes=0,written=0;
            Object.defineProperty(o.right,'value',{configurable:true,get(){reads++;o.left.value=100;$262.gc();return 3;}});
            update(o);assert(o.dst.value===13 && reads===1);
            o.dst={set value(v){writes++;written=v;}};
            update(o);assert(written===103 && reads===2 && writes===1);
            o.dst=new Proxy({},{set(t,k,v){writes++;written=v;return false;}});
            update(o);assert(written===103 && reads===3 && writes===2);
            var threw=false;try{strictUpdate(o);}catch(e){threw=e instanceof TypeError;}
            assert(threw && reads===4 && writes===3);
            o.dst=Object.freeze({value:0});threw=false;
            try{strictUpdate(o);}catch(e){threw=e instanceof TypeError;}
            assert(threw && o.dst.value===0 && reads===5);
            o.right={value:2};o.left.value={valueOf(){reads++;return 7;}};o.dst={value:0};
            update(o);assert(o.dst.value===9 && reads===6);
            o.dst=Object.create({set value(v){writes++;written=v;}});
            update(o);assert(written===9 && reads===7 && writes===4);
        "#,
        );
    }

    #[test]
    fn comparisons_handle_nan_and_nested_branches() {
        check(
            r#"
            function less(o){if(o.x<o.y)o.out=1;else o.out=2;}
            function le(o){if(o.x<=o.y)o.out=1;else o.out=2;}
            function greater(o){if(o.x>o.y)o.out=1;else o.out=2;}
            function ge(o){if(o.x>=o.y)o.out=1;else o.out=2;}
            function ne(o){if(o.x!=o.y)o.out=1;else o.out=2;}
            function nested(o){if(o.x<o.y){if(o.x<0)o.out=3;else o.out=4;}else o.out=5;}
            var o={x:1,y:2,out:0};
            for(var i=0;i<600;i++){less(o);assert(o.out===1);le(o);assert(o.out===1);greater(o);assert(o.out===2);ge(o);assert(o.out===2);ne(o);assert(o.out===1);nested(o);assert(o.out===4);}
            o.x=NaN;less(o);assert(o.out===2);le(o);assert(o.out===2);greater(o);assert(o.out===2);ge(o);assert(o.out===2);ne(o);assert(o.out===1);nested(o);assert(o.out===5);
            o.x=-1;nested(o);assert(o.out===3);o.x=2;ge(o);assert(o.out===1);le(o);assert(o.out===1);ne(o);assert(o.out===2);
        "#,
        );
    }
    #[test]
    fn named_global_roots_remain_live_and_getters_execute_once() {
        check(
            r#"
            globalThis.RegionDirection={limit:5};
            function update(o){if(o.x<RegionDirection.limit)o.dst.value=o.x+1;else o.dst.value=o.x-1;}
            var o={x:3,dst:{value:0}};
            for(var i=0;i<600;i++){update(o);assert(o.dst.value===4);}
            RegionDirection={limit:2};update(o);assert(o.dst.value===2);
            RegionDirection.extra=1;RegionDirection.limit=7;update(o);assert(o.dst.value===4);
            var gets=0;
            Object.defineProperty(globalThis,'RegionDirection',{configurable:true,get(){gets++;$262.gc();return {limit:1};}});
            update(o);assert(o.dst.value===2 && gets===1);
            delete globalThis.RegionDirection;
            var threw=false;try{update(o);}catch(e){threw=e instanceof ReferenceError;}
            assert(threw && o.dst.value===2);
        "#,
        );
    }

    #[test]
    fn cached_names_distinguish_closure_environments_and_fresh_tdz() {
        check(
            r#"
            function factory(limit){return function(o){if(o.x<limit)o.dst.value=o.x+1;else o.dst.value=o.x-1;};}
            function invoke(fn,o){fn(o);}
            var a=factory(5),b=factory(2),o={x:3,dst:{value:0}};
            for(var i=0;i<600;i++){invoke(a,o);assert(o.dst.value===4);}
            for(var i=0;i<100;i++){
                invoke(a,o);assert(o.dst.value===4);invoke(b,o);assert(o.dst.value===2);
            }
            $262.gc();invoke(a,o);assert(o.dst.value===4);invoke(b,o);assert(o.dst.value===2);
            function activation(){
                function update(o){if(o.x<limit)o.dst.value=o.x+1;else o.dst.value=o.x-1;}
                var t={x:3,dst:{value:99}},threw=false;
                try{update(t);}catch(e){threw=e instanceof ReferenceError;}
                assert(threw && t.dst.value===99);
                let limit=5;
                for(var i=0;i<150;i++){update(t);assert(t.dst.value===4);}
            }
            activation();activation();
        "#,
        );
    }

    #[test]
    fn cached_numeric_name_preserves_eighth_object_home_on_hit_and_miss() {
        // Current register allocation assigns o=x0, a=x1, ..., g=x7 in each
        // store expression. The scalar name comes after the destination chain and
        // must preserve x7 while the shared probe uses w7 as its representation flag.
        check(
            r#"
            globalThis.RegionScalar=6;
            function update(o){
                if(o.direction===1)o.a.b.c.d.e.f.g.value=RegionScalar+1;
                else o.a.b.c.d.e.f.g.value=RegionScalar-1;
            }
            var leaf={value:0};
            var o={direction:1,a:{b:{c:{d:{e:{f:{g:leaf}}}}}}};
            for(var i=0;i<600;i++){update(o);assert(leaf.value===7);}
            o.direction=0;update(o);assert(leaf.value===5);
            RegionScalar=10;update(o);assert(leaf.value===9);
            var gets=0;
            Object.defineProperty(globalThis,'RegionScalar',{configurable:true,get(){gets++;$262.gc();return 20;}});
            update(o);assert(leaf.value===19 && gets===1);
            Object.defineProperty(globalThis,'RegionScalar',{configurable:true,value:{valueOf(){gets++;return 30;}}});
            update(o);assert(leaf.value===29 && gets===2);
        "#,
        );
    }
}
