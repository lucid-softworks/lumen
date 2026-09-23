"use strict";

// Focused call-shape diagnostic. Build once, then run the release shell directly:
//
//   cargo build --release -p lumen --bin lumen
//   target/release/lumen --tier=jit --tier-threshold=0 \
//       crates/lumen/benches/call_dispatch_matrix.js

function measure(name, iterations, fn) {
    for (let i = 0; i < 3; i++) fn(10000);
    const start = Date.now();
    const value = fn(iterations);
    print(name + ": " + (Date.now() - start) + " ms (" + value + ")");
}

function add3(a, b, c) {
    return a + b + c;
}

function add1(x) { return x + 1; }
function add2(x) { return x + 2; }
function add4(x) { return x + 4; }
function add8(x) { return x + 8; }

const userTargets = [add1, add2, add4, add8];
const nativeTargets = [Math.abs, Math.floor, Math.ceil, Math.trunc];
const boundUser = add3.bind(null, 3, 4);
const boundNative = Math.max.bind(null, 3, 4);
const applyArgs = [3, 4, 5];
const spreadArgs = [3, 4, 5];

measure("native-direct", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += Math.abs(-(i & 7));
    return total;
});

measure("native-polymorphic-4", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += nativeTargets[i & 3](-(i & 7) - 0.25);
    return total;
});

measure("user-polymorphic-4", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += userTargets[i & 3](i & 7);
    return total;
});

measure("bound-user", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += boundUser(i & 7);
    return total;
});

measure("bound-native", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += boundNative(i & 7);
    return total;
});

measure("call-user", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += add3.call(null, 3, 4, i & 7);
    return total;
});

measure("call-native", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) total += Math.abs.call(null, -(i & 7));
    return total;
});

measure("apply-user", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) {
        applyArgs[2] = i & 7;
        total += add3.apply(null, applyArgs);
    }
    return total;
});

measure("apply-native", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) {
        applyArgs[2] = i & 7;
        total += Math.max.apply(null, applyArgs);
    }
    return total;
});

measure("spread-user", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) {
        spreadArgs[2] = i & 7;
        total += add3(...spreadArgs);
    }
    return total;
});

measure("spread-native", 2000000, n => {
    let total = 0;
    for (let i = 0; i < n; i++) {
        spreadArgs[2] = i & 7;
        total += Math.max(...spreadArgs);
    }
    return total;
});
