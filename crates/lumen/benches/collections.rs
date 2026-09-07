//! Warm collection workloads: define functions once, then reuse them across samples.
//! Run with `cargo bench -p lumen --bench collections`.
#[path = "support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen::Engine;

fn main() {
    let mut bench = Bench::new();
    for size in [100, 1_000, 10_000] {
        for kind in ["Map", "Set"] {
            let mut engine = Engine::new();
            let insert = if kind == "Map" { "set(i,i)" } else { "add(i)" };
            let lookup = if kind == "Map" {
                "get(i)===i"
            } else {
                "has(i)"
            };
            engine.eval(&format!(
                "function build() {{ let c=new {kind}(); for(let i=0;i<{size};i++) c.{insert}; return c.size; }}
                 let stored=new {kind}(); for(let i=0;i<{size};i++) stored.{insert};
                 function lookup() {{ let hits=0; for(let i=0;i<{size};i++) if(stored.{lookup}) hits++; return hits; }}"
            ), false).unwrap();
            for operation in ["build", "lookup"] {
                let source = format!("{operation}()");
                bench.run(&format!("{kind}/{operation}/{size}"), || {
                    black_box(engine.eval(black_box(&source), false).unwrap());
                });
            }
        }
    }
    bench.report();
}
