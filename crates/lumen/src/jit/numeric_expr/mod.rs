//! Acyclic numeric field expressions publish operands before an existing terminal instruction.
mod emit;
mod plan;
use crate::bytecode::Chunk;
use crate::jit::asm::Asm;
use crate::jit_ir::Cfg;
use crate::value::JitLayout;

pub(super) fn try_emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    pc: usize,
    labels: &[usize],
    targeted: &mut [bool],
    layout: &JitLayout,
) -> bool {
    if crate::jit::PACKED_LOCAL_SLOTS
        || !crate::jit::get_method_inlinable(layout)
        || layout.entry_accessor != layout.entry_value + 8
        || layout.rc_strong_off >= 256
        || std::env::var_os("LUMEN_JIT_NO_NUMERIC_EXPR").is_some()
    {
        return false;
    }
    let Some(plan) = plan::build(chunk, cfg, pc) else {
        return false;
    };
    // Retain every original instruction as a valid fallback/entry point, and protect
    // the existing terminal from other templates that fuse adjacent bytecodes.
    for flag in &mut targeted[pc..=plan.end] {
        *flag = true;
    }
    emit::emit(a, chunk, &plan, layout, labels);
    true
}

#[cfg(test)]
thread_local! {
    static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_success(a: &mut Asm) {
    a.mov_imm64(9, SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(10, 9, 0);
    a.add_imm(10, 10, 1);
    a.str_imm(10, 9, 0);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let script = format!(
                "function assert(v){{if(!v)throw new Error('numeric expression');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier == Tier::Jit {
                assert!(
                    super::SUCCESSES.with(|n| n.get()) > 0,
                    "expression never executed"
                );
            }
        }
    }

    #[test]
    fn arithmetic_fields_aliases_and_numeric_edge_values() {
        check(
            r#"
            function calculate(o){o.dst.value=o.src.value*o.scale.value+o.offset.value;}
            function reverse(o){o.dst.value=(o.src.value-o.offset.value)/o.scale.value;}
            function negate(o){o.dst.value=-o.src.value+2;}
            const o={dst:{value:0},src:{value:5},scale:{value:2},offset:{value:3}};
            for(let i=0;i<600;i++){calculate(o);assert(o.dst.value===13);reverse(o);assert(o.dst.value===1);}
            negate(o);assert(o.dst.value===-3);
            o.dst=o.src;calculate(o);assert(o.src.value===13);
            o.scale.value=0;o.offset.value=13;reverse(o);assert(Number.isNaN(o.dst.value));
            o.src.value=1;o.offset.value=0;reverse(o);assert(o.dst.value===Infinity);
            o.src.value=-0;o.scale.value=1;reverse(o);assert(Object.is(o.dst.value,-0));
            const a=[];a.value=7;o.src=a;o.dst={};o.scale.value=2;
            for(let i=0;i<100;i++){calculate(o);assert(o.dst.value===14);}
            a.push(1,2,3);calculate(o);assert(o.dst.value===14);
            a.length=0;calculate(o);assert(o.dst.value===14);
        "#,
        );
    }

    #[test]
    fn getters_proxies_and_coercions_resume_before_observable_reads() {
        check(
            r#"
            function calculate(o){o.dst.value=o.left.value+o.right.value;}
            const o={dst:{value:0},left:{value:10},right:{value:3}};
            for(let i=0;i<600;i++){calculate(o);assert(o.dst.value===13);}
            let reads=0;
            Object.defineProperty(o.right,'value',{configurable:true,get(){reads++;o.left.value=100;return 3;}});
            calculate(o);assert(o.dst.value===13 && reads===1);
            calculate(o);assert(o.dst.value===103 && reads===2);
            o.right=new Proxy({value:4},{get(t,k){reads++;return t[k];}});
            calculate(o);assert(o.dst.value===104 && reads===3);
            let coercions=0;o.left.value={valueOf(){coercions++;return 6;}};
            calculate(o);assert(o.dst.value===10 && coercions===1 && reads===4);
            o.left.value='x';o.right={value:2};calculate(o);assert(o.dst.value==='x2');
            o.left=Object.create({value:9});calculate(o);assert(o.dst.value===11);
        "#,
        );
    }

    #[test]
    fn existing_stores_keep_setters_proxy_traps_and_strict_failures() {
        check(
            r#"
            function calculate(o){o.dst.value=o.src.value*2+1;}
            function strictCalculate(o){'use strict';o.dst.value=o.src.value*2+1;}
            const o={dst:{value:0},src:{value:5}};
            for(let i=0;i<600;i++){calculate(o);strictCalculate(o);assert(o.dst.value===11);}
            let written=0,count=0;
            o.dst={set value(v){count++;written=v;o.src.value=7;}};
            calculate(o);assert(written===11 && count===1 && o.src.value===7);
            o.dst=new Proxy({},{set(t,k,v){count++;written=v;return false;}});
            calculate(o);assert(written===15 && count===2);
            let threw=false;try{strictCalculate(o);}catch(e){threw=e instanceof TypeError;}
            assert(threw && written===15 && count===3);
            o.dst=Object.freeze({value:0});threw=false;
            try{strictCalculate(o);}catch(e){threw=e instanceof TypeError;}
            assert(threw && o.dst.value===0);
            o.dst={};calculate(o);assert(o.dst.value===15);
        "#,
        );
    }

    #[test]
    fn returned_numeric_expressions_preserve_getters_and_coercion() {
        check(
            r#"
            function calculate(o){return o.left.value*o.scale.value+1;}
            const o={left:{value:4},scale:{value:3}};
            for(let i=0;i<600;i++)assert(calculate(o)===13);
            let gets=0;Object.defineProperty(o.scale,'value',{get(){gets++;return 2;}});
            assert(calculate(o)===9 && gets===1);
            let converted=0;o.left.value={valueOf(){converted++;return 5;}};
            assert(calculate(o)===11 && gets===2 && converted===1);
            o.left.value=-0;assert(calculate(o)===1);
        "#,
        );
    }

    #[test]
    fn locally_stored_numeric_expressions_preserve_handlers_and_captures() {
        check(
            r#"
            function calculate(o){var result=o.left.value*o.scale.value+1;return result;}
            const o={left:{value:4},scale:{value:3}};
            for(let i=0;i<600;i++)assert(calculate(o)===13);
            function captured(o){var result=o.left.value*2+1;return function(){return result;};}
            assert(captured(o)()===9);
            let gets=0;Object.defineProperty(o.scale,'value',{get(){gets++;throw o;}});
            function guarded(o){try{var result=o.left.value*o.scale.value+1;return result;}catch(e){return e;}}
            assert(guarded(o)===o && gets===1);
            let caught;try{calculate(o);}catch(e){caught=e;}assert(caught===o && gets===2);
        "#,
        );
    }

    #[test]
    fn implicit_destination_stores_preserve_existing_receiver_semantics() {
        check(
            r#"
            function calculate(o){o.result=o.left.value*2+1;}
            const o={result:0,left:{value:4}};
            for(let i=0;i<600;i++){calculate(o);assert(o.result===9);}
            let sets=0;Object.defineProperty(o,'result',{set(v){sets++;assert(v===9);}});
            calculate(o);assert(sets===1);
        "#,
        );
        check(
            r#"
            function calculate(){this.result=this.left.value*2+1;}
            const o={result:0,left:{value:4},calculate:calculate};
            for(let i=0;i<600;i++){o.calculate();assert(o.result===9);}
            let sets=0;Object.defineProperty(o,'result',{set(v){sets++;assert(v===9);}});
            o.calculate();assert(sets===1);
        "#,
        );
    }

    #[test]
    fn warmed_nested_wrappers_use_live_guarded_property_hints() {
        for body in [
            "return o.child.value+1;",
            "var result=o.child.value+1;return result;",
        ] {
            let mut engine = Engine::new();
            engine.set_tier(Tier::Jit);
            engine.set_tier_threshold(8);
            super::SUCCESSES.with(|n| n.set(0));
            crate::jit::property_probe::HINT_SUCCESSES.with(|n| n.set(0));
            // The first body ends at an inline-return Jump after expansion; the
            // second retains its local-store boundary. Both must use seeded hints.
            let source = r#"
                function read(o){BODY}
                function middle(o){return read(o);}
                function readSite(o){return middle(o);}
                var o={child:{value:7}};
                for(var i=0;i<600;i++)if(readSite(o)!==8)throw new Error('warm expression');
                o.child.value=9;if(readSite(o)!==10)throw new Error('live hinted value');
                'passed'
            "#
            .replace("BODY", body);
            match engine.eval(&source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed"),
                Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
            assert!(super::SUCCESSES.with(|n| n.get()) > 0, "{body}");
            assert!(
                crate::jit::property_probe::HINT_SUCCESSES.with(|n| n.get()) > 0,
                "{body}"
            );
        }
    }
}
