//! Engine benchmarks — running representative JavaScript through the public front door
//! (`Engine::eval`), plus engine startup. No internals: everything here is what an embedder or the
//! CLI actually calls.
//!
//! Run with `cargo bench -p lumen` (or `scripts/run-benches.sh`). Uses the std-only harness in
//! `support/harness.rs`; no third-party bench crate.

#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::Engine;

// ---- workloads (each an IIFE, so re-evaluating in one realm doesn't redeclare globals) --------

const FIB: &str = "(function fib(n){return n<2?n:fib(n-1)+fib(n-2)})(28)";

const ARITH_LOOP: &str = "(()=>{let s=0;for(let i=0;i<100000;i++){s+=i*2-1;s^=i&7;}return s})()";

const ARRAY_OPS: &str = "(()=>{\
    let a=[];for(let i=0;i<10000;i++)a.push(i*3);\
    let s=0;for(let i=0;i<a.length;i++)s+=a[i];\
    return a.map(x=>x+1).filter(x=>x&1).reduce((p,c)=>p+c,0)+s;\
})()";

const OBJECT_PROPS: &str = "(()=>{\
    let o={};for(let i=0;i<10000;i++)o['k'+(i&255)]=i;\
    let s=0;for(const k in o)s+=o[k];return s;\
})()";

const STRING_OPS: &str = "(()=>{\
    let parts=[];for(let i=0;i<5000;i++)parts.push('item-'+i);\
    let joined=parts.join(',');\
    return joined.split(',').filter(s=>s.length>6).length;\
})()";

const JSON_ROUNDTRIP: &str = "(()=>{\
    let o={items:[],meta:{n:0}};\
    for(let i=0;i<1000;i++)o.items.push({id:i,name:'n'+i,ok:(i&1)===0});\
    o.meta.n=o.items.length;\
    let s=JSON.stringify(o);return JSON.parse(s).items.length;\
})()";

const REGEX_OPS: &str = "(()=>{\
    let re=/([a-z]+)-(\\d+)/g;let text='';\
    for(let i=0;i<2000;i++)text+='item-'+i+' ';\
    let n=0,m;while((m=re.exec(text))!==null)n+=m[2].length;return n;\
})()";

const SORT_OPS: &str = "(()=>{\
    let a=[];let x=12345;\
    for(let i=0;i<5000;i++){x=(x*1103515245+12345)&0x7fffffff;a.push(x);}\
    a.sort((p,q)=>p-q);return a[0]+a[a.length-1];\
})()";

fn main() {
    let mut b = Bench::new();

    // Engine startup — what the CLI pays before running a line of user code.
    b.run("startup (Engine::new)", || {
        black_box(Engine::new());
    });

    // Each workload evaluated through Engine::eval on a reused realm — parse + run, the way real
    // JS is executed. The IIFE shape keeps the global clean across repetitions.
    let workloads = [
        ("fib(28)", FIB),
        ("arith-loop-100k", ARITH_LOOP),
        ("array-ops-10k", ARRAY_OPS),
        ("object-props-10k", OBJECT_PROPS),
        ("string-ops-5k", STRING_OPS),
        ("json-roundtrip", JSON_ROUNDTRIP),
        ("regex-exec-2k", REGEX_OPS),
        ("sort-5k", SORT_OPS),
    ];
    for (name, src) in workloads {
        let mut engine = Engine::new();
        b.run(name, || {
            black_box(engine.eval(black_box(src), false).unwrap());
        });
    }

    b.report();
}
