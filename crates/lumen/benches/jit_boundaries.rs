//! Focused benchmarks for ownership-sensitive JIT-to-runtime boundaries.

#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::Engine;

// Rotate through more receiver shapes than the polymorphic property cache can retain. Every
// access therefore enters the GetPropLocal helper, isolating its receiver ownership overhead.
const POLYMORPHIC_LOCAL_PROPS: &str = "(()=>{\
    const objects=[\
        {value:1},{a:0,value:2},{b:0,c:0,value:3},{d:0,e:0,f:0,value:4},\
        {g:0,h:0,i:0,j:0,value:5},{k:0,l:0,m:0,n:0,o:0,value:6},\
        {p:0,q:0,r:0,s:0,t:0,u:0,value:7},\
        {v:0,w:0,x:0,y:0,z:0,aa:0,ab:0,value:8}\
    ];\
    let sum=0;for(let i=0;i<100000;i++){let object=objects[i&7];sum+=object.value;}\
    return sum;\
})()";

fn main() {
    let mut bench = Bench::new();
    let mut engine = Engine::new();
    bench.run("polymorphic-local-props-100k", || {
        black_box(
            engine
                .eval(black_box(POLYMORPHIC_LOCAL_PROPS), false)
                .unwrap(),
        );
    });
    bench.report();
}
