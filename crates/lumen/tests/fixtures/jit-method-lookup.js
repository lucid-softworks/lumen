function assert(value, message) { if (!value) throw new Error(message); }
function invoke(o, value) { return o.method(value); }
function makeMethod(offset) { return function(value) { return this.base + value + offset; }; }
const own = {base: 10, method: makeMethod(1)};
for (let n = 0; n < 200; n++) assert(invoke(own, 2) === 13, 'warm own');
own.method = makeMethod(7);
assert(invoke(own, 2) === 19, 'same shape fresh closure');
let gets = 0;
Object.defineProperty(own, 'method', {
  get() { assert(this === own, 'getter receiver'); gets++; return makeMethod(9); },
  configurable: true
});
assert(invoke(own, 2) === 21 && gets === 1, 'live accessor exactly once');
delete own.method;
own.method = makeMethod(3);
assert(invoke(own, 2) === 15, 'delete and re-add');
assert(invoke({padding: 1, base: 20, method: makeMethod(2)}, 3) === 25, 'other shape');

// Independent sites populate caches at each supported prototype depth.
for (const depth of [1, 2, 3]) {
  const call = new Function('o', 'return o.method(4);');
  const holder = {method: makeMethod(5)};
  const mid = Object.create(holder);
  const receiver = Object.create(depth === 1 ? holder : depth === 2 ? mid : Object.create(mid));
  receiver.base = 30;
  for (let n = 0; n < 200; n++) assert(call(receiver) === 39, 'warm inherited');
  holder.method = makeMethod(8);
  assert(call(receiver) === 42, 'live inherited value');
  const replacement = {method: makeMethod(10)};
  if (depth === 1) Object.setPrototypeOf(receiver, replacement);
  else Object.setPrototypeOf(mid, replacement);
  assert(call(receiver) === 44, 'same shape replacement prototype');
  Object.defineProperty(replacement, 'method', {
    get() { assert(this === receiver, 'inherited getter receiver'); return makeMethod(12); },
    configurable: true
  });
  assert(call(receiver) === 46, 'inherited accessor');
  receiver.method = makeMethod(15); // Inherited getter has no setter: assignment has no effect.
  assert(call(receiver) === 46, 'inherited getter prevents assignment');
  delete replacement.method;
  receiver.method = makeMethod(16);
  assert(call(receiver) === 50, 'nearer method after deletion');
}

// Callee lookup happens before arguments. Removing its last property owner during argument
// evaluation must keep the selected function and original receiver alive on the operand stack.
function delayed(o, argument) { return o.method(argument()); }
const delayedReceiver = {base: 40, method: makeMethod(2)};
function normalArgument() { return 3; }
for (let n = 0; n < 200; n++) assert(delayed(delayedReceiver, normalArgument) === 45, 'warm delayed');
assert(delayed(delayedReceiver, function() {
  delete delayedReceiver.method;
  delayedReceiver.method = makeMethod(20);
  const garbage = [];
  for (let n = 0; n < 3000; n++) garbage.push({n, cycle: garbage});
  return 4;
}) === 46, 'callee owned across argument mutation');
assert(delayed(delayedReceiver, normalArgument) === 63, 'replacement selected next time');
function throwingArgument() { throw new RangeError('argument'); }
try { delayed(delayedReceiver, throwingArgument); assert(false, 'argument must throw'); }
catch (error) { assert(error instanceof RangeError, 'argument unwind'); }
assert(delayed(delayedReceiver, normalArgument) === 63, 'stack after unwind');

function invalid(o) { return o.method(); }
const changed = {method: function() { return this; }};
for (let n = 0; n < 200; n++) assert(invalid(changed) === changed, 'warm invalid site');
for (const value of [undefined, null, false, true, 0, -0, NaN, Infinity, 'text', Symbol('s'), 12n, {}]) {
  changed.method = value;
  let threw = false;
  try { invalid(changed); } catch (error) { threw = error instanceof TypeError; }
  assert(threw && Object.is(changed.method, value), 'non-callable value and ownership');
}
changed.method = function() { return this; };
assert(invalid(changed) === changed, 'recovered callable');
changed.method = function() { return this.base; }.bind({base: 77});
assert(invalid(changed) === 77, 'bound callable');
changed.method = changed;
try { invalid(changed); assert(false, 'alias must throw'); }
catch (error) { assert(error instanceof TypeError, 'callee receiver alias'); }
changed.method = function() { return this; };
assert(invalid(changed) === changed, 'alias recovery');

let proxyGets = 0;
const proxyTarget = {base: 80, method: makeMethod(3)};
const proxy = new Proxy(proxyTarget, {get(target, key, receiver) {
  if (key === 'method') proxyGets++;
  return Reflect.get(target, key, receiver);
}});
assert(invoke(proxy, 4) === 87 && proxyGets === 1, 'proxy fallback');
function text(o) { return o.toString(); }
const object = {toString: function() { return 'object'; }};
for (let n = 0; n < 200; n++) assert(text(object) === 'object', 'warm primitive site');
assert(text(42) === '42' && text('word') === 'word' && text(true) === 'true', 'primitive fallback');
for (const value of [null, undefined]) {
  let threw = false;
  try { text(value); } catch (error) { threw = error instanceof TypeError; }
  assert(threw, 'nullish fallback');
}
function stringCall(value) { return value.method(2); }
String.prototype.method = function(n) { return String(this) + n; };
for (let n = 0; n < 200; n++) assert(stringCall('a') === 'a2', 'warm string primitive');
String.prototype.method = function(n) { return String(this) + (n + 3); };
assert(stringCall('b') === 'b5', 'live string prototype value');
assert(stringCall(new String('c')) === 'c5', 'boxed string receiver');
let stringGets = 0;
Object.defineProperty(String.prototype, 'method', {
  get() { stringGets++; return function(n) { return String(this) + (n + 5); }; },
  configurable: true
});
assert(stringCall('d') === 'd7' && stringGets === 1, 'string getter once');
delete String.prototype.method;
let missingString = false;
try { stringCall('e'); } catch (error) { missingString = error instanceof TypeError; }
assert(missingString, 'deleted string method');

// Named array receiver keys are shape-stable; element entries and array holders are not.
function arrayCall(value) { return value.method(3); }
const arrayHolder = {method: function(n) { return this.length + n; }};
const arr = [1, 2];
Object.setPrototypeOf(arr, arrayHolder);
for (let n = 0; n < 200; n++) assert(arrayCall(arr) === 5, 'warm array receiver');
arr[100] = 7;
assert(arrayCall(arr) === 104, 'array element growth');
const larger = new Array(30);
Object.setPrototypeOf(larger, arrayHolder);
assert(arrayCall(larger) === 33, 'same named shape different element slots');
arrayHolder.method = function(n) { return this.length * n; };
assert(arrayCall(arr) === 303, 'live array prototype method');
arr.method = function(n) { return n + 1000; };
assert(arrayCall(arr) === 1003, 'array own method fallback');
delete arr.method;
assert(arrayCall(arr) === 303, 'array own method removed');
const otherArrayHolder = {method: function(n) { return this.length - n; }};
Object.setPrototypeOf(arr, otherArrayHolder);
assert(arrayCall(arr) === 98, 'array prototype swap');
let arrayGets = 0;
Object.defineProperty(otherArrayHolder, 'method', {
  get() { assert(this === arr, 'array getter receiver'); arrayGets++; return function(n) { return n; }; },
  configurable: true
});
assert(arrayCall(arr) === 3 && arrayGets === 1, 'array getter once');

// Primitive length and description retain special handling even when an object with the
// same prototype supplies a callable at those names during warmup.
function lengthCall(value) { return value.length(); }
const lengthObject = {length: function() { return 17; }};
for (let n = 0; n < 200; n++) assert(lengthCall(lengthObject) === 17, 'warm length');
for (const value of ['abc', new String('abc'), [1, 2]]) {
  let threw = false;
  try { lengthCall(value); } catch (error) { threw = error instanceof TypeError; }
  assert(threw, 'length remains noncallable');
}
// One invocation of a loop starts with an empty cache, learns an own method, then follows
// a different receiver through two prototype hops. Each live callee remains observable.
function oneInvocation(left, right) {
  let sum = 0;
  for (let n = 0; n < 500; n++) {
    if (n === 100) left.method = function(value) { return this.base + value + 2; };
    const receiver = n < 250 ? left : right;
    sum += receiver.method(n);
  }
  return sum;
}
const loopLeft = {base: 10, method: function(n) { return this.base + n; }};
const loopRight = Object.create(Object.create({method: function(n) { return this.base + n; }}));
loopRight.base = 20;
assert(oneInvocation(loopLeft, loopRight) === 132550, 'live cache in a single loop invocation');
function argumentOrder(o, argument) { return o.method(argument()); }
const ordered = {method: function(n) { return n; }};
for (let n = 0; n < 200; n++) assert(argumentOrder(ordered, normalArgument) === 3, 'warm argument order');
for (const value of [undefined, null, false, 1, 'text', Symbol('s'), 9n, {}]) {
  ordered.method = value;
  let argumentsRun = 0, threw = false;
  try {
    argumentOrder(ordered, function() {
      argumentsRun++;
      ordered.method = function(n) { return n; };
      return 1;
    });
  } catch (error) { threw = error instanceof TypeError; }
  assert(threw && argumentsRun === 1, 'noncallable selected before argument mutation');
}
ordered.method = undefined;
let argumentError = false;
try { argumentOrder(ordered, throwingArgument); }
catch (error) { argumentError = error instanceof RangeError; }
assert(argumentError, 'argument exception precedes noncallable TypeError');
// Alternate all four cached receiver shapes within one invocation, then change live
// values, descriptors and prototype links without changing the call/read sites.
function polymorphicBatch(objects, rounds) {
  let total = 0;
  for (let n = 0; n < rounds; n++) {
    const o = objects[n % 4];
    total += o.method() + o.value;
  }
  return total;
}
const polyOwn = {value: 1, method: function() { return this.value; }};
const polyOneHolder = {value: 2, method: function() { return this.value; }};
const polyOne = Object.create(polyOneHolder); polyOne.a = 1;
const polyTwoHolder = {value: 3, method: function() { return this.value; }};
const polyMiddle = Object.create(polyTwoHolder); polyMiddle.b = 1;
const polyTwo = Object.create(polyMiddle); polyTwo.c = 1; polyTwo.d = 1;
const polyOther = {x: 0, y: 0, z: 0, value: 4, method: function() { return this.value; }};
const polymorphic = [polyOwn, polyOne, polyTwo, polyOther];
assert(polymorphicBatch(polymorphic, 4000) === 20000, 'four cache ways');
polyOwn.value = 5;
polyOneHolder.value = 6;
polyTwoHolder.value = 7;
polyOther.value = 8;
assert(polymorphicBatch(polymorphic, 4000) === 52000, 'four live values');
let polyGets = 0;
Object.defineProperty(polyOneHolder, 'value', {get() { polyGets++; return 9; }, configurable: true});
assert(polymorphicBatch(polymorphic, 400) === 5800 && polyGets === 200, 'polymorphic accessor fallback');
Object.setPrototypeOf(polyMiddle, {value: 10, method: function() { return this.value + 1; }});
assert(polymorphicBatch(polymorphic, 400) === 6500, 'polymorphic middle prototype swap');
delete polyOther.method;
Object.setPrototypeOf(polyOther, {method: function() { return this.value + 2; }});
assert(polymorphicBatch(polymorphic, 400) === 6700, 'polymorphic deletion and inherited replacement');
'passed';
