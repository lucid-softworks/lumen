"use strict";

function branchBool(value, number) {
    if (value) return number;
    return number + 1;
}

function run(count) {
    let sum = 0;
    for (let i = 0; i < count; i++) sum += branchBool((i & 1) === 1, i & 7);
    return sum;
}

run(10000);
const count = 100000000;
const start = Date.now();
const checksum = run(count);
print("bool-branch: " + (Date.now() - start) + " ms (" + checksum + ")");
