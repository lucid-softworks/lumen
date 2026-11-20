var asyncDone = false, asyncError;
function assert(value) { if (!value) throw new Error('hoist assertion'); }
function nested(seed) {
  var {left: [alpha, , ...tail], right: {value: π}, ...rest} = {
    left: [seed, 99, seed + 1], right: {value: seed + 2}, extra: seed + 3
  };
  function read() { return alpha + tail[0] + π + rest.extra; }
  var alpha;
  return read;
}
const closures = [];
for (let i = 0; i < 80; i++) closures.push(nested(i));
for (let i = 0; i < closures.length; i++) assert(closures[i]() === 4 * i + 6);
function ordered(input) {
  var f;
  function f() { return 1; }
  function f() { return input; }
  return f();
}
for (let i = 0; i < 80; i++) assert(ordered(i) === i);
function loopHeads(input) {
  var sum = 0;
  for (var [key, value] of [[1, input], [2, input + 1]]) sum += key + value;
  for (var index = 0; index < 2; index++) sum += index;
  for (var field in {a: 1}) sum += field.length;
  return [sum, key, value, index, field].join(',');
}
for (let i = 0; i < 80; i++) assert(loopHeads(i) === [2*i+6,2,i+1,2,'a'].join(','));
const sloppy = new Function('input', `
  var before = typeof promoted;
  if (input) { function promoted() { return input; } }
  return [before, typeof promoted, input ? promoted() : 0].join(',');
`);
for (let i = 0; i < 80; i++) assert(sloppy(i) === (i ? 'undefined,function,'+i : 'undefined,undefined,0'));
const blocked = new Function('input', `
  var outer = 'outer';
  { let outer = input; { function outer() { return 2; } } }
  return outer;
`);
for (let i = 0; i < 80; i++) assert(blocked(i) === 'outer');
function withDefault(value = 3) {
  var value;
  var longerNameThatMustKeepItsFullContents = value;
  return () => longerNameThatMustKeepItsFullContents;
}
assert(withDefault()() === 3);
for (let i = 0; i < 80; i++) assert(withDefault(i)() === i);


function* retained(seed) {
  var local = seed;
  yield () => local;
  local += 1;
  yield () => local;
}
const generators = Array.from({length:32}, (_,i)=>retained(i));
const reads = generators.map(g=>g.next().value);
for(let i=0;i<32;i++) assert(reads[i]()===i);
for(let i=31;i>=0;i--) {
  const read=generators[i].next().value;
  assert(read()===i+1 && reads[i]()===i+1);
  assert(generators[i].next().done);
}
function* blockedGenerator(name) {
  { function name() { return 'block'; } }
  yield name;
}
for(let i=0;i<40;i++) assert(blockedGenerator(i).next().value===i);
function* strictGenerator(value) {
  'use strict';
  { function local() { return value; } }
  yield typeof local;
}
for(let i=0;i<40;i++) assert(strictGenerator(i).next().value==='undefined');
async function deferred(seed) {
  with ({}) {} // Exercise the interpreter fallback, including suspension and resumption.
  var local = seed;
  await 0;
  return () => local;
}
Promise.all(Array.from({length:32},(_,i)=>deferred(i))).then(values=>{
  for(let i=0;i<32;i++) assert(values[i]()===i);
}).then(() => { asyncDone = true; }, error => {
  asyncDone = true; asyncError = String(error && error.stack || error);
});
