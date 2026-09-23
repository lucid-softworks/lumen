"use strict";

// Focused call-dispatch diagnostic. Run a release shell directly so compilation is excluded:
//
//   cargo build --release -p lumen --bin lumen
//   target/release/lumen --tier=jit --tier-threshold=0 \
//       crates/lumen/benches/call_dispatch.js
//
// Each workload performs two million stable operations; the other call shapes are controls for
// constructor-only changes. The printed checksum prevents accidentally timing dead work.

function measure(name, fn) {
    for (let i = 0; i < 3; i++) fn(10000);
    const start = Date.now();
    const value = fn(2000000);
    print(name + ": " + (Date.now() - start) + " ms (" + value + ")");
}

function add3(a, b, c) {
    return a + b + c;
}

class Counter {
    constructor(value) {
        this.value = value;
    }

    add(delta) {
        return this.value + delta;
    }
}

class Pair {
    constructor(left, right) {
        this.left = left;
        this.right = right;
    }
}

function recursiveSum(n) {
    if (n === 0) return 0;
    return n + recursiveSum(n - 1);
}

measure("direct", n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += add3(i & 7, 2, 3);
    return total;
});

const counter = new Counter(7);
measure("method", n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += counter.add(i & 7);
    return total;
});

measure("construct", n => {
    let total = 0;
    for (let i = 0; i < n; i++) {
        const pair = new Pair(i & 7, 3);
        total += pair.left + pair.right;
    }
    return total;
});

measure("native", n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += Math.abs(-(i & 7));
    return total;
});

measure("recursive", n => {
    let total = 0;
    for (let i = 0; i < n / 100; i++) total += recursiveSum(30);
    return total;
});
