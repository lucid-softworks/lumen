//! Descriptor-based array writes share the dense entry map with indexed operations.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn indexed_descriptors_remain_correct_across_holes_sparse_keys_and_slot_shifts() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine
            .eval(
                r#"
            function assert(v) { if (!v) throw new Error('descriptor mismatch'); }
            function exercise(a) {
                for (let i=0;i<512;i++) Object.defineProperty(a,String(i),{
                    value:i,writable:true,enumerable:true,configurable:true
                });
                a[1000000]=9;
                a['01']=11;
                a.label=12;
                delete a[0]; delete a[250];
                Object.defineProperty(a,'251',{get(){return 42},configurable:true});
                Object.defineProperty(a,'250',{value:7,writable:false,configurable:true});
                assert(!Object.hasOwn(a,'0') && a[250]===7 && a[251]===42);
                assert(a[249]===249 && a[252]===252 && a[1000000]===9);
                assert(a['01']===11 && a.label===12);
                assert(Object.getOwnPropertyDescriptor(a,'250').writable===false);
                if (Array.isArray(a)) {
                    a.length=256;
                    assert(!Object.hasOwn(a,'256') && !Object.hasOwn(a,'1000000'));
                    a[0]=99;
                    assert(a[0]===99 && a[255]===255 && a.label===12);
                }
            }
            exercise([]); exercise({}); exercise([1,2,3]);
            'passed'
        "#,
                false,
            )
            .unwrap();
        match result {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
