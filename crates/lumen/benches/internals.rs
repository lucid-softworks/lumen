//! Internal-stage benchmarks — the compilation pipeline behind cold-boot cost, timed stage by
//! stage: **lex → parse → snapshot-encode → snapshot-decode**. Reaches internals through the
//! `bench` feature's `lumen::bench_api` (not the stable public surface).
//!
//! Run with `cargo bench -p lumen --features bench` (or `scripts/run-benches.sh`).

#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::bench_api::{decode, encode, parse_script, tokenize};

/// A small but feature-dense script (classes, async, destructuring, generators, arrows).
const KITCHEN_SINK: &str = concat!(
    "class Point{constructor(x,y){this.x=x;this.y=y;}dist(){return Math.hypot(this.x,this.y);}}\n",
    "const pts=Array.from({length:100},(_,i)=>new Point(i,i*2));\n",
    "async function sum(xs){let t=0;for await(const x of xs)t+=x;return t;}\n",
    "const f=(a,b=1,...rest)=>a+b+rest.reduce((p,c)=>p+c,0);\n",
    "const {a,b,...c}={a:1,b:2,d:3,e:4};\n",
    "const g=function*(){yield* [1,2,3];};\n",
    "for(const p of pts){if(p.dist()>10)break;}\n",
);

/// Build a larger, realistic-shaped program (many small functions) to show stage scaling.
fn large_source() -> String {
    let mut s = String::with_capacity(64 * 1024);
    for i in 0..500 {
        s.push_str(&format!(
            "function handler{i}(req, res) {{\n  const id = req.params.id * {i} + 1;\n  \
             const items = req.body.items.filter(x => x.kind === 'a{i}');\n  \
             return res.json({{ id, count: items.length, ok: id > {i} }});\n}}\n"
        ));
    }
    s
}

fn stage_group(b: &mut Bench, label: &str, src: &str) {
    b.run(&format!("{label}: lex"), || {
        black_box(tokenize(black_box(src)).unwrap_or_else(|_| panic!("lex")));
    });
    b.run(&format!("{label}: parse (lex+parse)"), || {
        black_box(parse_script(black_box(src), false).unwrap_or_else(|_| panic!("parse")));
    });

    // Snapshot encode/decode operate on an already-parsed AST.
    let body = parse_script(src, false).unwrap_or_else(|_| panic!("parse"));
    let bytes = encode(&body);
    b.run(&format!("{label}: snapshot-encode"), || {
        black_box(encode(black_box(&body)));
    });
    b.run(&format!("{label}: snapshot-decode"), || {
        black_box(decode(black_box(&bytes)).unwrap_or_else(|_| panic!("decode")));
    });
}

fn main() {
    let mut b = Bench::new();
    let large = large_source();

    stage_group(&mut b, "kitchen-sink", KITCHEN_SINK);
    stage_group(&mut b, "large-500fn", &large);

    b.report();
}
