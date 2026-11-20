const accepted = [
  'function f(a) { var a; function a() {} }',
  'function f(a) { { let a; } class other {} }',
  'function f([]) { let a; } function g({}) { const b = 1; }',
  'const f = (a) => { var a; function a() {} };',
  'class C { method(a) { var a; } set value(a) { { let a; } } }',
  'function f(a) { for (let a of []) {} }',
  'async function f(a) { function a() {} }',
  'function* f(a) { function a() {} }',
  'function f(π) { function π() {} }',
];
const rejected = [
  'function f(a, b) { let b; let a; }',
  'function f({a, b}) { class b {} let a; }',
  'function f([a, ...b]) { const b = 1; }',
  'function f({a, ...b}) { let b; }',
  'function f(a, ...b) { const b = 1; let a; }',
  'async function f(a, b = 1) { let b; let a; }',
  'function* f(a, b) { let b; let a; }',
  'const f = (a, b) => { let b; let a; };',
  'class C { method(a, b) { let b; let a; } }',
  'class C { set value(a) { let a; } }',
  'function f(π) { let π; }',
  'function f(a) { let \\u0061; }',
  'function f(a) { function a() {} let a; }',
  'function f(a) { using a = null; }',
  'async function f(a) { await using a = null; }',
];
for (const source of accepted) new Function(source);
for (const source of rejected) {
  let rejected = false;
  try { new Function(source); } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    rejected = true;
  }
  if (!rejected) throw Error('accepted conflicting binding: ' + source);
}
let declarations = '';
for (let i = 0; i < 600; i++) declarations += 'function name' + i + '() {}\n';
for (const params of ['', 'name0', '[]', '{}']) new Function(params, declarations);
const run = new Function('a', 'function a() { return 7; } return a();');
if (run(3) !== 7) throw Error('function binding did not replace parameter');
