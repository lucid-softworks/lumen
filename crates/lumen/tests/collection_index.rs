//! Collection behavior through the public engine boundary, including compiled calls and callbacks.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let source = format!(
            "function assert(ok) {{ if (!ok) throw new Error('assertion failed'); }}
             (function() {{ {source} return 'ok'; }})()"
        );
        match engine.eval(&source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "ok", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn same_value_zero_and_identity_keys() {
    check(
        r#"
        const a={}, b={}, s=Symbol('x'), t=Symbol('x');
        const keys=[undefined,null,false,true,0,NaN,1,1n,'1',a,b,s,t,
                    123456789012345678901234567890n,'😀','\ud800'];
        const m=new Map(), set=new Set();
        keys.forEach((key,i)=>{m.set(key,i);set.add(key)});
        assert(m.size===keys.length && set.size===keys.length);
        keys.forEach((key,i)=>assert(m.get(key)===i && set.has(key)));
        assert(m.get(-0)===4 && m.get(0/0)===5);
        assert(m.get(BigInt('123456789012345678901234567890'))===13);
        assert(m.get(String.fromCodePoint(0x1f600))===14);
        assert(m.get(String.fromCharCode(0xd800))===15);
        m.set(-0,42); set.add(-0); m.set(0/0,43); set.add(0/0);
        assert(m.size===keys.length && set.size===keys.length);
        assert(m.get(0)===42 && m.get(NaN)===43);
        assert(Object.is([...new Map([[-0,1]]).keys()][0],0));
        assert(Object.is([...new Set([-0])][0],0));
        assert(!m.has({}) && !set.has(Symbol('x')));
    "#,
    );
}

#[test]
fn live_iterators_survive_delete_clear_and_reinsert() {
    check(
        r#"
        for(const C of [Map,Set]) {
            const c=new C();
            function add(k) { if(C===Map)c.set(k,k);else c.add(k); }
            add(1);add(2);add(3);
            const iter=c.keys(), unstarted=c.keys();
            assert(iter.next().value===1);
            c.delete(2);add(2);
            assert(iter.next().value===3);
            assert(iter.next().value===2);
            c.clear(); c.clear(); add(4);
            assert(iter.next().value===4);
            assert(unstarted.next().value===4);
            assert(iter.next().done);
            add(5);
            assert(iter.next().done);
            assert([...c.keys()].join(',')==='4,5');
            assert(c.size===2 && !c.has(1) && !c.has(2));
        }
    "#,
    );
}

#[test]
fn foreach_observes_mutation_without_revisiting_updates() {
    check(
        r#"
        for(const C of [Map,Set]) {
            const c=new C(), seen=[];
            function add(k) { if(C===Map)c.set(k,k);else c.add(k); }
            add(1);add(2);add(3);
            c.forEach((value,key)=>{
                seen.push(key);
                if(key===1){ c.delete(2);add(1);add(2); }
                if(key===3){ c.clear();add(4); }
            });
            assert(seen.join(',')==='1,3,4');
            assert(c.size===1 && c.has(4));
        }
    "#,
    );
}

#[test]
fn computed_insertion_rechecks_callback_mutations() {
    check(
        r#"
        for(const C of [Map,WeakMap]) {
            const key={}, other={}, m=new C();
            m.set(other,0);
            assert(m.getOrInsertComputed(key,k=>{assert(k===key);m.set(k,1);return 2})===2);
            assert(m.get(key)===2);
            assert(m.getOrInsertComputed(key,()=>{throw new Error('must not call')})===2);
            m.delete(key);
            assert(m.getOrInsertComputed(key,k=>{
                m.set(k,3);m.delete(k);m.set(k,4);return 5;
            })===5);
            assert(m.get(key)===5 && m.getOrInsert(key,6)===5);
        }
        const m=new Map();
        m.getOrInsertComputed(-0,k=>{assert(Object.is(k,0));m.clear();m.set(k,1);return 2});
        assert(m.size===1 && m.get(0)===2);
    "#,
    );
}

#[test]
fn growth_deletion_and_bulk_construction_keep_indexes_consistent() {
    check(
        r#"
        const m=new Map(), s=new Set();
        for(let i=0;i<2048;i++){ m.set('k'+i,i);s.add('k'+i); }
        for(let i=0;i<2048;i+=2){ assert(m.delete('k'+i));assert(s.delete('k'+i)); }
        assert(m.size===1024 && s.size===1024);
        for(let i=0;i<2048;i++)assert(m.has('k'+i)===(i%2===1) && s.has('k'+i)===(i%2===1));
        for(let i=0;i<2048;i+=2){m.set('k'+i,-i);s.add('k'+i)}
        assert([...m.keys()].join(',')===[...s].join(','));
        assert(m.size===2048 && m.get('k2046')===-2046);
        const grouped=Map.groupBy([1,2,3,4],n=>n%2);
        assert(grouped.get(1).join(',')==='1,3' && grouped.get(0).join(',')==='2,4');
        grouped.set(1,'updated');assert(grouped.size===2 && grouped.get(1)==='updated');
        const a=new Set([1,2,3]);a.delete(2);
        const union=a.union(new Set([3,4]));
        assert([...union].join(',')==='1,3,4' && union.has(4));
        assert([...a.intersection(new Set([3,4]))].join(',')==='3');
    "#,
    );
}

#[test]
fn weak_collections_preserve_key_validation_and_identity() {
    check(
        r#"
        const a={}, b={}, symbol=Symbol(), m=new WeakMap(), s=new WeakSet();
        for(const k of [a,b,symbol]){m.set(k,42);s.add(k)}
        assert(m.get(a)===42 && m.has(b) && s.has(symbol));
        assert(m.delete(a) && s.delete(a) && !m.delete(a) && !s.delete(a));
        assert(m.has(b) && s.has(b));
        for(const k of [1,'x',null,Symbol.for('registered')]) {
            let throws=0;
            try {m.set(k,1)} catch(e){assert(e instanceof TypeError);throws++}
            try {s.add(k)} catch(e){assert(e instanceof TypeError);throws++}
            assert(throws===2 && !m.has(k) && !s.has(k));
        }
    "#,
    );
}
