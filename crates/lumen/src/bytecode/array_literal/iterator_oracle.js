function assert(value) { if (!value) throw new Error('literal iterator protocol'); }
function equal(value, expected) { assert(JSON.stringify(value) === JSON.stringify(expected)); }
function throwsName(fn, name) {
  let seen = false;
  try { fn(); } catch (error) { seen = error.name === name; }
  assert(seen);
}
function iterator(values) {
  let index = 0;
  return { next() { return index < values.length ? { value: values[index++], done: false } : { done: true }; } };
}
function copy(source) { return [...source]; }
function fallback(source) { return [, ...source]; }
for (let round = 0; round < 20; round++) {
  let reads = 0;
  const own = [1];
  Object.defineProperty(own, Symbol.iterator, { get() { reads++; return function () { return iterator([99]); }; } });
  equal(copy(own), [99]);
  assert(reads === 1);
  const hole = fallback(own);
  assert(!(0 in hole) && hole.length === 2 && hole[1] === 99 && reads === 2);

  const arrayMethod = Array.prototype[Symbol.iterator];
  try {
    Array.prototype[Symbol.iterator] = function () { return iterator([7]); };
    equal(copy([1, 2]), [7]);
    equal(copy('ab'), ['a', 'b']);
  } finally { Array.prototype[Symbol.iterator] = arrayMethod; }

  const stringMethod = String.prototype[Symbol.iterator];
  try {
    String.prototype[Symbol.iterator] = function () { return iterator([8]); };
    equal(copy('abc'), [8]);
  } finally { String.prototype[Symbol.iterator] = stringMethod; }
  equal(copy('a😀b'), ['a', '😀', 'b']);

  const arrayIteratorProto = Object.getPrototypeOf([][Symbol.iterator]());
  const next = arrayIteratorProto.next;
  try {
    arrayIteratorProto.next = function () {
      const step = next.call(this);
      if (!step.done) step.value += 10;
      return step;
    };
    equal(copy([1, 2]), [11, 12]);
  } finally { arrayIteratorProto.next = next; }

  const wide = [];
  for (let index = 0; index < 32; index++) wide.push(index);
  equal(copy(wide), wide);
  const wideCustom = wide.slice();
  wideCustom[Symbol.iterator] = function () { return iterator([99]); };
  equal(copy(wideCustom), [99]);
  const wideGrowing = wide.slice();
  Object.defineProperty(wideGrowing, '0', { get() { wideGrowing.push(32); return 0; } });
  const wideResult = copy(wideGrowing);
  assert(wideResult.length === 33 && wideResult[0] === 0 && wideResult[32] === 32);

  const growing = [1];
  Object.defineProperty(growing, '0', { get() { growing.push(2); return 1; } });
  equal(copy(growing), [1, 2]);
  const shrinking = [1, 2];
  Object.defineProperty(shrinking, '0', { get() { shrinking.length = 1; return 1; } });
  equal(copy(shrinking), [1]);

  let nextRead = false;
  Object.defineProperty(Number.prototype, 'next', { configurable: true, get() { nextRead = true; return function () { return { done: true }; }; } });
  try {
    throwsName(function () { copy({ [Symbol.iterator]() { return 1; } }); }, 'TypeError');
    assert(!nextRead);
  } finally { delete Number.prototype.next; }
  let closes = 0;
  throwsName(function () {
    copy({ [Symbol.iterator]() { return { next() { throw new RangeError('step'); }, return() { closes++; return {}; } }; } });
  }, 'RangeError');
  assert(closes === 0);
}
'passed';
