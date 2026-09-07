//! Call forwarding preserves target, receiver, environment and throw semantics.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if(!x) throw new Error('assertion'); }}\nfunction runChecks() {{ {source} }} runChecks();\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn polymorphic_targets_closures_and_exceptions() {
    check(
        r#"
        function invoke(f, receiver, value) { return f.call(receiver, value); }
        function plus(v) { return this.n + v; }
        function minus(v) { return this.n - v; }
        function factory(n) { return function(v) { return this.n + n + v; }; }
        function fail(v) { throw v; }
        const receiver={n:20};
        for(let i=0;i<1000;i++) {
            assert(invoke(plus,receiver,2)===22);
            assert(invoke(minus,receiver,2)===18);
            assert(invoke(factory(i),receiver,2)===22+i);
            try { invoke(fail,receiver,receiver); assert(false); }
            catch(e) { assert(e===receiver); }
        }
    "#,
    );
}

#[test]
fn builtin_override_proxy_and_strict_receiver() {
    check(
        r#"
        function invoke(f, receiver, value) { return f.call(receiver, value); }
        function target(v) { 'use strict'; return this === undefined ? v : this; }
        for(let i=0;i<300;i++) assert(invoke(target,undefined,7)===7);
        target.call=function(r,v) { return v+1; };
        assert(invoke(target,null,7)===8);
        delete target.call;
        assert(invoke(target,null,7)===null);
        const proxy=new Proxy(target,{apply(f,r,a) { return a[0]+2; }});
        assert(invoke(proxy,undefined,7)===9);
        assert(invoke(target,3,7)===3);
        assert(invoke(Math.abs,undefined,-7)===7);
        const bound=target.bind(undefined);
        assert(invoke(bound,null,7)===7);
        assert(invoke((v)=>v+3,null,7)===10);
    "#,
    );
}

#[test]
fn recursive_forwarding_and_argument_ownership() {
    check(
        r#"
        function invoke(f,r,a,b,c) { return f.call(r,a,b,c); }
        function identity(a,b,c) { return [this,a,b,c]; }
        const r={},a={},b={},c={};
        for(let i=0;i<500;i++) {
            const out=invoke(identity,r,a,b,c);
            assert(out[0]===r && out[1]===a && out[2]===b && out[3]===c);
        }
        function recurse(n) { return n ? recurse.call(this,n-1)+1 : this.n; }
        for(let i=0;i<300;i++) assert(recurse.call({n:2},20)===22);
    "#,
    );
}

#[test]
fn guard_misses_preserve_lexical_new_target_and_foreign_realms() {
    check(
        r#"
        function invoke(f,r,v) { return f.call(r,v); }
        function ordinary(v) { return v+1; }
        for(let i=0;i<500;i++) assert(invoke(ordinary,null,2)===3);
        function C() {
            const arrow=()=>new.target;
            assert(invoke(arrow,null,0)===C);
        }
        new C();
        const realm=$262.createRealm();
        realm.evalScript('function foreign() { return this; }');
        assert(invoke(realm.global.foreign,undefined,0)===realm.global);
        assert(invoke(ordinary,null,2)===3);
    "#,
    );
}

#[test]
fn optimized_constructor_keeps_prototype_return_and_target_guards() {
    check(
        r#"
        let replacement={replacement:true};
        function Base(v) { this.value=v; }
        function C(v) { Base.call(this,v); if(v<0) return replacement; }
        for(let i=0;i<500;i++) assert(new C(i).value===i);
        const proto={}; C.prototype=proto;
        let result=new C(4);
        assert(result.value===4 && Object.getPrototypeOf(result)===proto);
        assert(new C(-1)===replacement);
        Base=function(v) { this.value=v+2; };
        assert(new C(4).value===6);
        Base.call=function(receiver,v) { receiver.value=v+3; };
        assert(new C(4).value===7);
        delete Base.call;
        let seen=0;
        Object.defineProperty(proto,'value',{set(v) { seen=v; },configurable:true});
        result=new C(4);
        assert(seen===6 && !Object.hasOwn(result,'value'));
    "#,
    );
}

#[test]
fn forwarded_calls_preserve_observable_caller_frames_after_warmup() {
    check(
        r#"
        function target() { return target.caller===invoke; }
        function invoke(f) { return f.call(null); }
        for(let i=0;i<500;i++) assert(invoke(target));
    "#,
    );
}
