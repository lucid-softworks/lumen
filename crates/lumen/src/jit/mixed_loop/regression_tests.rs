//! Observable fallback transitions after verified native loop execution.
use crate::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) {
    match engine.eval(source, false).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn transition(warm: &str, change: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        super::ITERATIONS.with(|n| n.set(0));
        eval(
            &mut engine,
            "function assert(v){if(!v)throw new Error('mixed transition');}",
        );
        eval(&mut engine, warm);
        if tier == Tier::Jit {
            assert!(
                super::ITERATIONS.with(|n| n.get()) >= 32,
                "no sustained native warmup"
            );
        }
        eval(&mut engine, change);
    }
}

const WARM_SCAN: &str = r#"
    function scan(a,n){var sum=0,c;for(var i=0;i<n;i++){c=a[i];c.value=c.value+1;sum+=c.value;}return sum;}
    function fresh(){var a=[];for(var i=0;i<40;i++)a.push({value:1});return a;}
    var a=fresh();for(var j=0;j<8;j++)assert(scan(a,40)===40*(j+2));
"#;

#[test]
fn dense_hole_inherited_getter_publishes_prior_writes_before_gc() {
    transition(
        WARM_SCAN,
        r#"
        a=fresh();var calls=0;
        var proto=Object.create(Array.prototype);
        Object.defineProperty(proto,'10',{get(){
            calls++;assert(a[0].value===2&&a[9].value===2);$262.gc();return {value:7};
        }});
        Object.setPrototypeOf(a,proto);delete a[10];
        assert(scan(a,40)===86&&calls===1);
        assert(a[0].value===2&&a[39].value===2);
    "#,
    );
}

#[test]
fn inherited_field_accessor_observes_exact_receiver_and_prior_state() {
    transition(
        WARM_SCAN,
        r#"
        a=fresh();var reads=0,writes=0,written=0;
        var proto={};
        Object.defineProperty(proto,'value',{
            get(){reads++;assert(this===a[10]);assert(a[9].value===2);$262.gc();return 5;},
            set(v){writes++;written=v;assert(this===a[10]);}
        });
        a[10]=Object.create(proto);
        assert(scan(a,40)===83&&reads===2&&writes===1&&written===6);
        assert(a[0].value===2&&a[39].value===2);
    "#,
    );
}

#[test]
fn warmed_method_getter_and_prototype_replacement_preserve_dispatch() {
    transition(
        r#"
        function A(){this.value=0;}function B(){this.value=0;}
        A.prototype.run=function(){this.value=this.value+1;};
        B.prototype.run=function(){this.value=this.value+2;};
        function execute(a){for(var i=0;i<a.length;i++){var c=a[i];c.run();}}
        function invoke(a){execute(a);}
        var a=[];for(var i=0;i<12;i++)a.push(i%2?new A():new B());
        for(var j=0;j<600;j++)invoke(a);
        assert(a[0].value===1200&&a[1].value===600);
    "#,
        r#"
        var gets=0,runs=0;
        Object.defineProperty(A.prototype,'run',{configurable:true,get(){
            gets++;$262.gc();return function(){runs++;this.value=this.value+10;};
        }});
        invoke(a);
        assert(gets===6&&runs===6&&a[0].value===1202&&a[1].value===610);
        Object.setPrototypeOf(a[0],{run:function(){this.value=this.value+20;}});
        invoke(a);
        assert(gets===12&&runs===12&&a[0].value===1222&&a[1].value===620);
    "#,
    );
}

#[test]
fn warmed_readonly_write_throws_after_exact_prior_effects() {
    transition(
        r#"
        function scan(a,n){'use strict';var c;for(var i=0;i<n;i++){c=a[i];c.value=c.value+1;}}
        var a=[];for(var i=0;i<40;i++)a.push({value:1});
        for(var j=0;j<8;j++)scan(a,40);
        assert(a[0].value===9&&a[39].value===9);
    "#,
        r#"
        Object.defineProperty(a[10],'value',{writable:false});
        var caught=false;
        try{scan(a,40);}catch(e){caught=e instanceof TypeError;}
        assert(caught&&a[0].value===10&&a[9].value===10);
        assert(a[10].value===9&&a[11].value===9&&a[39].value===9);
    "#,
    );
}
