function assert(value, message) { if (!value) throw new Error(message); }
function C(value) { this.x = value; this.y = value + 1; }
const parent = {x: 0};
C.prototype = Object.create(parent);
for (let index = 0; index < 200; index++) new C(index);
Object.defineProperty(parent, 'x', {writable: false});
const locked = new C(7);
assert(!Object.hasOwn(locked, 'x') && locked.x === 0 && locked.y === 8, 'deep nonwritable');
Object.defineProperty(parent, 'x', {writable: true});
assert(new C(9).x === 9, 'writable restored');
for (let index = 0; index < 200; index++) new C(index);
let seen = 0;
Object.defineProperty(parent, 'x', {set(value) { seen += value; }, configurable: true});
const intercepted = new C(11);
assert(seen === 11 && !Object.hasOwn(intercepted, 'x'), 'deep accessor');
delete parent.x;
for (let index = 0; index < 200; index++) new C(index);
Object.defineProperty(C.prototype, 'x', {value: 42, writable: false, configurable: true});
assert(new C(13).x === 42, 'new closer shadow');
delete C.prototype.x;
for (let index = 0; index < 200; index++) new C(index);
const differentParent = {set x(value) { seen += value; }};
Object.setPrototypeOf(C.prototype, differentParent);
const swapped = new C(17);
assert(seen === 28 && !Object.hasOwn(swapped, 'x'), 'prototype swap');

// A species callback can return an object that already serves as a cached prototype.
const speciesTarget = {};
function build() { this.marker = 5; }
build.prototype = speciesTarget;
for (let index = 0; index < 200; index++) new build();
const values = [1];
values.constructor = {[Symbol.species]: function() { return speciesTarget; }};
values.map(value => value);
Object.defineProperty(speciesTarget, 'marker', {value: 8, writable: false});
assert(new build().marker === 8, 'species destination becomes nonwritable shadow');

// DefineOwnProperty must invalidate indexed append proofs on either standard prototype.
function append(array, value) { array.push(value); return array; }
for (let index = 0; index < 200; index++) append([], index);
let arrayWrites = 0;
Object.defineProperty(Array.prototype, '0', {configurable: true, set(value) { arrayWrites += value; }});
try {
    const changed = append([], 19);
    assert(arrayWrites === 19 && changed.length === 1 && !Object.hasOwn(changed, '0'), 'array prototype setter');
} finally { delete Array.prototype[0]; }
for (let index = 0; index < 200; index++) append([], index);
Object.defineProperty(Object.prototype, '0', {configurable: true, value: 23, writable: false});
try {
    let caught = false;
    try { append([], 29); } catch (error) { caught = error instanceof TypeError; }
    assert(caught, 'object prototype nonwritable index');
} finally { delete Object.prototype[0]; }
assert(append([], 31)[0] === 31, 'append restored');
'passed';
