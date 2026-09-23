// A fresh sibling closure shares its caller's lexical environment on every batch.
function factory(seed) {
  function leaf(x) { return seed + x; }
  function batch() {
    let total = 0;
    for (let j = 0; j < 32; j++) total += leaf(j);
    return total;
  }
  return batch;
}
function run() {
  let total = 0;
  for (let i = 0; i < 10000; i++) total += factory(i)();
  if (total !== 1604800000) throw new Error('wrong sum: ' + total);
}
function warm() {
  const batch = factory(0);
  for (let i = 0; i < 150; i++) if (batch() !== 496) throw new Error('warm sum');
}
warm();
run();
const start = Date.now();
for (let k = 0; k < 5; k++) run();
console.log('SharedClosureBatch:', Date.now() - start, 'ms');
