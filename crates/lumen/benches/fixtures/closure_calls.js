function factory(seed) { return function(x) { return seed + x; }; }
function invoke(f, x) { return f(x); }
function run() {
  let total = 0;
  for (let i = 0; i < 10000; i++) {
    const f = factory(i);
    for (let j = 0; j < 32; j++) total += invoke(f, j);
  }
  if (total !== 1604800000) throw new Error('wrong sum: ' + total);
}
run();
const start = Date.now();
for (let k = 0; k < 5; k++) run();
console.log('ClosureBatch:', Date.now() - start, 'ms');
