//! A tiny std-only benchmark harness — no third-party crate (lumen takes none, anywhere).
//!
//! It does what criterion/divan do at the core: warm up, auto-calibrate an iteration count so a
//! sample runs long enough to time precisely, take the *minimum* per-op time over several samples
//! (the least-noise estimate for a deterministic microbenchmark), and print a table.
//!
//! Bench files opt out of the default (unstable) libtest harness with `harness = false` and call
//! [`Bench::run`] / [`Bench::report`] from their own `main`.

#![allow(dead_code)]

use std::time::{Duration, Instant};

/// Keep the optimizer from eliding work whose result is unused.
pub fn black_box<T>(v: T) -> T {
    std::hint::black_box(v)
}

/// One measured benchmark: its name, best per-op time (ns), and how many iterations backed the
/// estimate.
struct Row {
    name: String,
    ns_per_op: f64,
    iters: u64,
}

pub struct Bench {
    rows: Vec<Row>,
    /// Wall-clock budget per benchmark for the sampling phase.
    sample_target: Duration,
    samples: u32,
}

impl Default for Bench {
    fn default() -> Bench {
        Bench::new()
    }
}

impl Bench {
    pub fn new() -> Bench {
        Bench {
            rows: Vec::new(),
            sample_target: Duration::from_millis(25),
            samples: 25,
        }
    }

    /// Time `f`, reporting nanoseconds per call. The closure is run many times per sample; put a
    /// [`black_box`] around anything whose result would otherwise be optimized away.
    pub fn run<F: FnMut()>(&mut self, name: &str, mut f: F) {
        // Warm up caches / branch predictors / lazy inits.
        for _ in 0..5 {
            f();
        }

        // Calibrate: grow the per-sample iteration count until one sample reaches the target
        // duration, so timing resolution isn't the bottleneck.
        let mut iters: u64 = 1;
        loop {
            let elapsed = time_batch(&mut f, iters);
            if elapsed >= self.sample_target || iters >= 1 << 32 {
                break;
            }
            let target_ns = self.sample_target.as_nanos().max(1);
            let elapsed_ns = elapsed.as_nanos().max(1);
            let grow = (target_ns / elapsed_ns).clamp(2, 1000) as u64;
            iters = iters.saturating_mul(grow);
        }

        // Sample: keep the fastest per-op time (min = the run least perturbed by the OS).
        let mut best = f64::INFINITY;
        for _ in 0..self.samples {
            let elapsed = time_batch(&mut f, iters);
            let ns = elapsed.as_nanos() as f64 / iters as f64;
            if ns < best {
                best = ns;
            }
        }

        self.rows.push(Row {
            name: name.to_string(),
            ns_per_op: best,
            iters,
        });
    }

    /// Print the collected results as an aligned table.
    pub fn report(&self) {
        let name_w = self
            .rows
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(4)
            .max(9);
        println!();
        println!(
            "{:<name_w$}  {:>12}  {:>14}  {:>12}",
            "benchmark", "time/op", "ops/sec", "iters"
        );
        println!("{}", "-".repeat(name_w + 2 + 12 + 2 + 14 + 2 + 12));
        for r in &self.rows {
            let ops_per_sec = if r.ns_per_op > 0.0 {
                1e9 / r.ns_per_op
            } else {
                0.0
            };
            println!(
                "{:<name_w$}  {:>12}  {:>14}  {:>12}",
                r.name,
                fmt_time(r.ns_per_op),
                fmt_count(ops_per_sec),
                fmt_count(r.iters as f64),
            );
        }
        println!();
    }
}

fn time_batch<F: FnMut()>(f: &mut F, iters: u64) -> Duration {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed()
}

/// Render a nanosecond duration with an adaptive unit.
fn fmt_time(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.1} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// Render a large count with a K/M/G suffix.
fn fmt_count(n: f64) -> String {
    if n < 1_000.0 {
        format!("{n:.0}")
    } else if n < 1_000_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else if n < 1_000_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else {
        format!("{:.1}G", n / 1_000_000_000.0)
    }
}
