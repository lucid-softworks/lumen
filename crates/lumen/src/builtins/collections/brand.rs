//! Collection brands are internal data slots, independent of ordinary JS properties.
use crate::builtins::collection_data::CollectionKind;
use crate::interpreter::Interp;
use crate::value::Value;
use std::rc::Rc;

pub(in crate::builtins) fn coll_ptr(i: &Interp, this: &Value) -> Result<usize, Value> {
    coll_ptr_kind(i, this, None)
}

/// Resolve an exact strong collection brand, or either Map/Set when `want` is absent.
pub(in crate::builtins) fn coll_ptr_kind(
    i: &Interp,
    this: &Value,
    want: Option<&str>,
) -> Result<usize, Value> {
    let err = || i.make_error("TypeError", "method called on an incompatible receiver");
    let object = this.as_obj().ok_or_else(err)?;
    let ptr = Rc::as_ptr(object) as usize;
    let kind = i.map_data.get(&ptr).ok_or_else(err)?.kind();
    let valid = match want {
        Some(name) => kind.name() == name,
        None => matches!(kind, CollectionKind::Map | CollectionKind::Set),
    };
    if valid {
        Ok(ptr)
    } else {
        Err(err())
    }
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script=format!("function assert(x) {{if(!x)throw new Error('assertion');}} function rejects(f) {{let yes=false;try{{f();}}catch(e){{yes=e instanceof TypeError;}}assert(yes);}} function drive() {{{source}}} drive();'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }

    #[test]
    fn properties_cannot_supply_or_change_collection_slots() {
        check(
            r#"
            const ctors=[Map,Set,WeakMap,WeakSet],names=['Map','Set','WeakMap','WeakSet'],key={};
            const methods=[['get','has','set','delete','clear','keys'],['has','add','delete','clear','keys'],['get','has','set','delete'],['has','add','delete']];
            for(let i=0;i<ctors.length;i++) {
                const c=new ctors[i]();assert(Reflect.ownKeys(c).length===0);
                if(i%2)c.add(key);else c.set(key,17);
                c.__ck='different';assert(c.has(key));
                delete c.__ck;
                Object.defineProperty(c,'__ck',{get(){throw 'marker getter';}});
                assert(c.has(key));if(i%2===0)assert(c.get(key)===17);
                for(let j=0;j<ctors.length;j++)if(i!==j) {
                    for(const name of methods[j]) {
                        const method=ctors[j].prototype[name];
                        rejects(()=>method.call(c,key,key));
                    }
                }
                const fake=Object.create(ctors[i].prototype);fake.__ck=names[i];
                rejects(()=>fake.has(key));
            }
        "#,
        );
    }

    #[test]
    fn subclass_and_new_target_prototypes_do_not_define_the_brand() {
        check(
            r#"
            class M extends Map{} class S extends Set{}
            class WM extends WeakMap{} class WS extends WeakSet{}
            const key={},m=new M([[key,11]]),s=new S([key]),wm=new WM([[key,12]]),ws=new WS([key]);
            for(const c of [m,s,wm,ws])assert(Reflect.ownKeys(c).length===0 && c.has(key));
            assert(m.get(key)===11 && wm.get(key)===12);
            const map=Reflect.construct(Map,[],Set);
            Map.prototype.set.call(map,key,13);
            const set=Reflect.construct(Set,[],Map);
            Set.prototype.add.call(set,key);
            assert(Object.getPrototypeOf(map)===Set.prototype && Map.prototype.get.call(map,key)===13);
            assert(Object.getPrototypeOf(set)===Map.prototype && Set.prototype.has.call(set,key));
            rejects(()=>Set.prototype.has.call(map,key));rejects(()=>Map.prototype.has.call(set,key));
            m.clear();s.clear();assert(m.set(key,14)===m && s.add(key)===s);
            Object.setPrototypeOf(m,Set.prototype);
            assert(Map.prototype.get.call(m,key)===14);rejects(()=>Set.prototype.has.call(m,key));
        "#,
        );
    }

    #[test]
    fn collection_factories_create_the_correct_internal_slot() {
        check(
            r#"
            const grouped=Map.groupBy([1,2,3],v=>v%2);
            assert(Reflect.ownKeys(grouped).length===0 && grouped.get(1).join(',')==='1,3');
            grouped.__ck='Set';assert(grouped.get(0)[0]===2);
            rejects(()=>Set.prototype.has.call(grouped,1));
            const a=new Set([1,2]),b=new Set([2,3]);
            for(const name of ['union','intersection','difference','symmetricDifference']) {
                const result=a[name](b);assert(Reflect.ownKeys(result).length===0);
                result.__ck='Map';assert(result.add(17)===result && result.has(17));
                rejects(()=>Map.prototype.has.call(result,17));
                result.clear();assert(result.add(18)===result && result.has(18));
            }
        "#,
        );
    }

    #[test]
    fn weak_brands_survive_compaction_and_cross_realm_calls() {
        check(
            r#"
            const keys=Array.from({length:100},()=>({})),m=new WeakMap(),s=new WeakSet();
            for(const key of keys){m.set(key,key);s.add(key);}
            for(let i=0;i<90;i++){assert(m.delete(keys[i]));assert(s.delete(keys[i]));}
            assert(m.get(keys[99])===keys[99] && s.has(keys[99]));
            rejects(()=>WeakMap.prototype.has.call(s,keys[99]));
            rejects(()=>WeakSet.prototype.delete.call(m,keys[99]));
            const realm=$262.createRealm(),foreign=new realm.global.WeakMap([[keys[99],17]]);
            assert(WeakMap.prototype.get.call(foreign,keys[99])===17);
            assert(realm.global.WeakSet.prototype.has.call(s,keys[99]));
            let threw=false;try{realm.global.WeakSet.prototype.has.call(m,keys[99]);}
            catch(e){threw=e instanceof realm.global.TypeError;}assert(threw);
            assert(m.has(keys[99]) && s.has(keys[99]));
        "#,
        );
    }
}
