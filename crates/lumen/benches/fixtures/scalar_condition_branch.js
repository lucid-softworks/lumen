"use strict";

function branchReturn(value) {
    if (value & 1) return value;
    return value + 1;
}

function run(count) {
    let sum = 0;
    for (let i = 0; i < count; i++) sum += branchReturn(i & 7);
    return sum;
}

run(10000);
const count = 100000000;
const start = Date.now();
const checksum = run(count);
print("branch-return: " + (Date.now() - start) + " ms (" + checksum + ")");
