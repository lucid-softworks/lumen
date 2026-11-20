function check(ok, message) { if (!ok) throw new Error(message); }
function invoke(fn, a, b, c) { return fn(a, b, c); }
function add(a, b, c) { return a + b + c; }
function subtract(a, b, c) { return a - b - c; }
function multiply(a, b, c) { return a * b * c; }
const choices = [add, subtract, multiply, Math.max];
for (let i = 0; i < 800; i++) {
    const index = i % 4;
    const expected = [9, 1, 15, 5][index];
    check(invoke(choices[index], 5, 3, 1) === expected, 'four-way dispatch');
}
const bound = add.bind(null, 7);
check(invoke(bound, 2, 3, 99) === 12, 'bound fallback');
const proxy = new Proxy(add, { apply(target, receiver, args) { return target(...args) + 10; } });
for (let i = 0; i < 80; i++) {
    check(invoke(proxy, 5, 3, 1) === 19, 'proxy fallback');
    check(invoke(choices[i % 4], 5, 3, 1) === [9, 1, 15, 5][i % 4], 'cached call after proxy');
}
function method(receiver, value) { return receiver.run(value); }
function strictMethod(value) { 'use strict'; return this.base + value; }
const receivers = [{base: 1, run: strictMethod}, {base: 2, run: strictMethod}];
for (let i = 0; i < 300; i++) check(method(receivers[i % 2], 5) === 6 + i % 2, 'live this');
const marker = { message: 'expected' };
function mayThrow(value) { if (value % 3 === 0) throw marker; return value + 1; }
for (let i = 0; i < 300; i++) {
    let result;
    try { result = invoke(mayThrow, i, 0, 0); check(i % 3 !== 0 && result === i + 1, 'return'); }
    catch (error) { check(i % 3 === 0 && error === marker, 'throw identity'); }
}
let current = add;
function swap() { current = subtract; return 5; }
function capturedCallee() { return current(swap(), 3, 1); }
for (let i = 0; i < 80; i++) { current = add; check(capturedCallee() === 9, 'callee captured before argument'); }
function closure(value) { return function captured(a) { return value + a; }; }
for (let i = 0; i < 500; i++) check(invoke(closure(i), 1, 0, 0) === i + 1, 'fresh closure environment');
let effects = 0;
function argument() { effects++; return { retained: effects }; }
for (let i = 0; i < 40; i++) {
    let caught = false;
    try { invoke(null, argument(), 0, 0); } catch (error) { caught = error instanceof TypeError; }
    check(caught && effects === i + 1, 'non-callable argument order');
}
function sloppyThis() { return this === globalThis; }
function strictThis() { 'use strict'; return this === undefined; }
for (let i = 0; i < 100; i++) {
    check(invoke(sloppyThis) === true && invoke(strictThis) === true, 'bare-call this');
}
function many(a0,a1,a2,a3,a4,a5,a6,a7,a8,a9,a10,a11,a12,a13,a14,a15,a16,a17,a18,a19) { return a0 + a19; }
for (let i = 0; i < 80; i++) check(many(1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20) === 21, 'large argument stack offset');
'passed';
