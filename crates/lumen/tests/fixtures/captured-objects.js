function assert(value) { if (!value) throw new Error('captured object binding'); }
function throwsName(fn, name) {
  let caught = false;
  try { fn(); } catch (error) { caught = error.name === name; }
  assert(caught);
}

function compiled_simple(source) {
  let { value } = source;
  return function (delta) { value += delta; return value; };
}
function compiled_order(create) {
  function readA() { return a; }
  function readB() { return b; }
  const { a, b } = create(readA, readB);
  return [readA, readB];
}
function compiled_nested(source) {
  let { 'left': { a }, right: b } = source;
  return function (delta) { a += delta; return a + b; };
}
function compiled_const(source) {
  const { a } = source;
  return function (value) { a = value; };
}
function compiled_partial(source, publish) {
  function readA() { return a; }
  publish(readA);
  const { a, b } = source;
  return function () { return b; };
}
function fallback_array(source) {
  let [a, b] = source;
  return function () { return a + b; };
}
function fallback_default(source) {
  let { a = 17 } = source;
  return function () { return a; };
}
function fallback_rest(source) {
  const { a, ...rest } = source;
  return function () { return rest.b; };
}
function fallback_computed(source, key) {
  const { [key]: a } = source;
  return function () { return a; };
}

for (let round = 0; round < 40; round++) {
  const one = compiled_simple({ value: round });
  const two = compiled_simple({ value: round + 100 });
  assert(one(2) === round + 2);
  assert(two(3) === round + 103);
  assert(one(1) === round + 3);
  const events = [];
  const readers = compiled_order(function (readA, readB) {
    throwsName(readA, 'ReferenceError');
    throwsName(readB, 'ReferenceError');
    return {
      get a() {
        events.push('a');
        throwsName(readA, 'ReferenceError');
        throwsName(readB, 'ReferenceError');
        return round;
      },
      get b() {
        events.push('b');
        assert(readA() === round);
        throwsName(readB, 'ReferenceError');
        return round + 1;
      }
    };
  });
  assert(events.join(',') === 'a,b');
  assert(readers[0]() === round && readers[1]() === round + 1);
  assert(compiled_nested({ left: { a: round }, right: 3 })(2) === round + 5);
  throwsName(compiled_const({ a: 1 }), 'TypeError');
  let saved;
  throwsName(function () {
    compiled_partial({ a: round, get b() { throw new RangeError('stop'); } }, function (read) { saved = read; });
  }, 'RangeError');
  assert(saved() === round);
  throwsName(function () { compiled_simple(null); }, 'TypeError');
  throwsName(function () { compiled_nested({ left: undefined, right: 3 }); }, 'TypeError');
  assert(fallback_array([2, 3])() === 5);
  assert(fallback_default({})() === 17);
  assert(fallback_rest({ a: 1, b: 2 })() === 2);
  assert(fallback_computed({ a: 4 }, 'a')() === 4);
}
'passed';
