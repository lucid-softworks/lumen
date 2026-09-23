//! Focused typed-array store and DataView access workloads.
//!
//! Run with `cargo bench -p lumen --bench typed_array_stores`. Each case uses a warmed function
//! that crosses the engine's element/helper boundary repeatedly; the tier labels make it possible
//! to compare the tree-walker against the configured compiled tier.

#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::bytecode::Tier;
use lumen::Engine;

fn run_case(bench: &mut Bench, name: &str, tier: Tier, setup: &str, call: &str) {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.eval(setup, false).unwrap();
    bench.run(name, || {
        black_box(engine.eval(black_box(call), false).unwrap());
    });
}

fn main() {
    let mut bench = Bench::new();
    let typed_numeric = concat!(
        "var typed=new Uint32Array(4096);",
        "function storeTyped(){for(var i=0;i<typed.length;i++)typed[i]=i+1;",
        "return typed[4095];}",
    );
    run_case(
        &mut bench,
        "interp/typed-store-u32-4096",
        Tier::Interp,
        typed_numeric,
        "storeTyped()",
    );
    run_case(
        &mut bench,
        "jit/typed-store-u32-4096",
        Tier::Jit,
        typed_numeric,
        "storeTyped()",
    );

    let typed_bigint = concat!(
        "var typed=new BigInt64Array(4096);",
        "function storeTyped(){for(var i=0;i<typed.length;i++)typed[i]=BigInt(i);",
        "return typed[4095];}",
    );
    run_case(
        &mut bench,
        "interp/typed-store-bigint64-4096",
        Tier::Interp,
        typed_bigint,
        "storeTyped()",
    );
    run_case(
        &mut bench,
        "jit/typed-store-bigint64-4096",
        Tier::Jit,
        typed_bigint,
        "storeTyped()",
    );

    let dataview_numeric = concat!(
        "var view=new DataView(new ArrayBuffer(8192));",
        "function storeView(){for(var i=0;i<4096;i++)view.setUint16(i*2,i,true);",
        "return view.getUint16(8190,true);}",
    );
    run_case(
        &mut bench,
        "interp/dataview-set-u16-le-4096",
        Tier::Interp,
        dataview_numeric,
        "storeView()",
    );
    run_case(
        &mut bench,
        "jit/dataview-set-u16-le-4096",
        Tier::Jit,
        dataview_numeric,
        "storeView()",
    );

    let dataview_big_endian = concat!(
        "var view=new DataView(new ArrayBuffer(8192));",
        "function storeView(){for(var i=0;i<4096;i++)view.setUint16(i*2,i,false);",
        "return view.getUint16(8190,false);}",
    );
    run_case(
        &mut bench,
        "interp/dataview-set-u16-be-4096",
        Tier::Interp,
        dataview_big_endian,
        "storeView()",
    );
    run_case(
        &mut bench,
        "jit/dataview-set-u16-be-4096",
        Tier::Jit,
        dataview_big_endian,
        "storeView()",
    );

    let dataview_bigint = concat!(
        "var view=new DataView(new ArrayBuffer(32768));",
        "function storeView(){for(var i=0;i<4096;i++)view.setBigInt64(i*8,BigInt(i),false);",
        "return view.getBigInt64(32760,false);}",
    );
    run_case(
        &mut bench,
        "interp/dataview-set-bigint64-be-4096",
        Tier::Interp,
        dataview_bigint,
        "storeView()",
    );
    run_case(
        &mut bench,
        "jit/dataview-set-bigint64-be-4096",
        Tier::Jit,
        dataview_bigint,
        "storeView()",
    );

    bench.report();
}
