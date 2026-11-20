function assert(value) { if (!value) throw new Error('array literal spread'); }
function throwsName(fn, name) {
  let caught = false;
  try { fn(); } catch (error) { caught = error.name === name; }
  assert(caught);
}
function compiled_copy(source) { return [...source]; }
function compiled_runs(source, mark) {
  return [mark('p1'), mark('p2'), ...source, mark('s1'), mark('s2')];
}
function compiled_mixed(first, second, mark) {
  return [mark('before'), ...first, mark('middle'), ...second, mark('after')];
}
function compiled_captured(source) {
  const { values } = source;
  return [...values, function () { return values; }];
}
function fallback_holes(source) { return [, ...source]; }
for (let round = 0; round < 40; round++) {
  const events = [];
  const iterable = {
    get [Symbol.iterator]() {
      events.push('iterator');
      return function () {
        events.push('open');
        let count = 0;
        return {
          get next() {
            events.push('next-method');
            return function () {
              events.push('step');
              const current = count++;
              return {
                get done() { events.push('done'); return current === 2; },
                get value() { events.push('value'); return current + round; }
              };
            };
          }
        };
      };
    }
  };
  const result = compiled_mixed(iterable, [7, 8], function (name) { events.push(name); return name; });
  assert(result.join(',') === 'before,' + round + ',' + (round + 1) + ',middle,7,8,after');
  assert(events.join(',') === 'before,iterator,open,next-method,step,done,value,step,done,value,step,done,middle,after');
  const calls = [];
  const runs = compiled_runs([2], function (name) { calls.push(name); return name; });
  assert(runs.join(',') === 'p1,p2,2,s1,s2');
  assert(calls.join(',') === 'p1,p2,s1,s2');
  let afterThrow = false;
  throwsName(function () {
    compiled_runs([2], function (name) {
      if (name === 's1') throw new RangeError('batch stop');
      if (name === 's2') afterThrow = true;
      return name;
    });
  }, 'RangeError');
  assert(!afterThrow);
  const chars = compiled_copy('a😀b');
  assert(chars.length === 3 && chars[1] === '😀');
  const sparse = compiled_copy([, 3]);
  assert(sparse.length === 2 && 0 in sparse && sparse[0] === undefined);
  const source = { values: [round] };
  const captured = compiled_captured(source);
  assert(captured.length === 2 && captured[0] === round && captured[1]() === source.values);
  const holes = fallback_holes([5]);
  assert(holes.length === 2 && !(0 in holes) && holes[1] === 5);
  throwsName(function () { compiled_copy(null); }, 'TypeError');
  throwsName(function () { compiled_copy({ [Symbol.iterator]: 4 }); }, 'TypeError');
  let later = false;
  const bad = { [Symbol.iterator]() { return { next() { throw new RangeError('stop'); } }; } };
  throwsName(function () { compiled_mixed(bad, [], function (name) { if (name === 'middle') later = true; return name; }); }, 'RangeError');
  assert(!later);
}
let setterCalls = 0;
Object.defineProperty(Array.prototype, '0', { configurable: true, set(value) { setterCalls++; } });
let own;
try { own = compiled_copy([9]); } finally { delete Array.prototype[0]; }
assert(setterCalls === 0 && own[0] === 9 && Object.prototype.hasOwnProperty.call(own, '0'));
'passed';
