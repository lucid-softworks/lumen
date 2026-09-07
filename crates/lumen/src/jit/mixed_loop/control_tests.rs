//! Backedge counters count inner and outer backward jumps, not outer iterations.
use crate::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        super::ENTRIES.with(|n| n.set(0));
        super::ITERATIONS.with(|n| n.set(0));
        super::POST_WRITE_EXITS.with(|n| n.set(0));
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let script = format!(
            "function assert(v){{if(!v)throw new Error('mixed control');}} {source}; 'passed'"
        );
        match engine.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
        if tier == Tier::Jit {
            let entries = super::ENTRIES.with(|n| n.get());
            let backedges = super::ITERATIONS.with(|n| n.get());
            assert!(entries > 0, "no native entry");
            assert!(
                backedges > 1024 && backedges > entries * 2,
                "insufficient native backward jumps: {backedges}/{entries}"
            );
            assert!(
                super::POST_WRITE_EXITS.with(|n| n.get()) > 0,
                "no publication after a native heap write"
            );
        }
    }
}

#[test]
fn nested_budget_exits_preserve_outer_locals_continue_and_break() {
    check(
        r#"
        function scan(a){
            var sum=0,c;
            for(var outer=0;outer<3;outer++){
                for(var inner=0;inner<1400;inner++){
                    c=a[inner];
                    if(inner===5)continue;
                    c.value=c.value+1;
                    sum+=c.value;
                    if(inner===1300)break;
                }
            }
            return sum;
        }
        var a=[];for(var k=0;k<1400;k++)a.push({value:0});
        assert(scan(a)===7800);
        for(var k=0;k<1400;k++)assert(a[k].value===(k<=1300&&k!==5?3:0));
        assert(scan(a)===19500);
        assert(a[0].value===6&&a[5].value===0&&a[1300].value===6&&a[1301].value===0);
    "#,
    );
}

#[test]
fn cold_call_publishes_prior_writes_and_pending_destination_and_number() {
    check(
        r#"
        var calls=0;
        function cold(o){calls++;assert(o.value===1);$262.gc();return 7;}
        function scan(a){
            var c;
            for(var i=0;i<a.length;i++){
                c=a[i];c.value=c.value+1;
                if(i===1300)c.out.value=100+cold(c);
            }
        }
        var a=[];for(var k=0;k<1400;k++)a.push({value:0,out:{value:0}});
        scan(a);assert(calls===1);
        for(var k=0;k<1400;k++){
            assert(a[k].value===1);
            assert(a[k].out.value===(k===1300?107:0));
        }
    "#,
    );
}
