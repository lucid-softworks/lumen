//! Folded literal keys retain object-definition order and descriptor semantics.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if(!x) throw new Error('assertion'); }}\n{source}\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn computed_proto_is_data_and_duplicate_order_is_preserved() {
    check(
        r#"
        function make(v) { return {['b']:1,['1']:2,[('a')]:3,['b']:v,['__proto__']:v}; }
        const value={};
        for(let i=0;i<300;i++) {
            const o=make(value);
            assert(Object.getPrototypeOf(o)===Object.prototype);
            assert(o.__proto__===value && o.b===value && Object.keys(o).join(',')==='1,b,a,__proto__');
            const d=Object.getOwnPropertyDescriptor(o,'__proto__');
            assert(d.value===value && d.writable && d.enumerable && d.configurable);
        }
    "#,
    );
}

#[test]
fn names_unicode_and_value_evaluation_order() {
    check(
        r#"
        let trace='';
        function make() { return {['x']:(trace+='a',1),['😀']:(trace+='b',2),['fn']:()=>3}; }
        for(let i=0;i<300;i++) {
            trace=''; const o=make();
            assert(trace==='ab' && o.x===1 && o['😀']===2 && o.fn.name==='fn' && o.fn()===3);
        }
        function dynamic() { return {[(trace+='k','x')]:(trace+='v',1)}; }
        trace=''; assert(dynamic().x===1 && trace==='kv');
    "#,
    );
}
