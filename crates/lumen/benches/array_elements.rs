//! Focused array and element-access workloads.
//!
//! Run with `cargo bench -p lumen --bench array_elements`. Each function is compiled once and
//! invoked through `Engine::eval`, so the rows measure the warmed execution path rather than
//! repeatedly compiling the workload body.

#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::Engine;

fn run_case(bench: &mut Bench, name: &str, setup: &str, call: &str) {
    let mut engine = Engine::new();
    engine.eval(setup, false).unwrap();
    bench.run(name, || {
        black_box(engine.eval(black_box(call), false).unwrap());
    });
}

fn main() {
    let mut bench = Bench::new();
    let packed = concat!(
        "var packed=[];for(var i=0;i<4096;i++)packed.push(i);",
        "function packedRead(){var sum=0;for(var i=0;i<packed.length;i++)sum+=packed[i];return sum;}",
        "function packedStore(){for(var i=0;i<packed.length;i++)packed[i]=i+1;return packed[4095];}",
        "function packedLength(){var sum=0;for(var i=0;i<10000;i++)sum+=packed.length;return sum;}",
    );
    run_case(&mut bench, "packed/read-4096", packed, "packedRead()");
    run_case(&mut bench, "packed/store-4096", packed, "packedStore()");
    run_case(&mut bench, "packed/length-10k", packed, "packedLength()");

    let holey = concat!(
        "var holey=new Array(4096);for(var i=0;i<4096;i+=2)holey[i]=i;",
        "function holeyRead(){var sum=0;for(var i=0;i<holey.length;i++)sum+=holey[i]||0;return sum;}",
    );
    run_case(&mut bench, "holey/read-4096", holey, "holeyRead()");

    let iterator = concat!(
        "var iterable=[];for(var i=0;i<4096;i++)iterable.push(i);",
        "function iterate(){var sum=0;for(var value of iterable)sum+=value;return sum;}",
    );
    run_case(&mut bench, "iterator/values-4096", iterator, "iterate()");

    let typed = concat!(
        "var typed=new Uint32Array(4096);for(var i=0;i<typed.length;i++)typed[i]=i;",
        "function typedRead(){var sum=0;for(var i=0;i<typed.length;i++)sum+=typed[i];return sum;}",
    );
    run_case(&mut bench, "typed/read-4096", typed, "typedRead()");

    bench.report();
}
