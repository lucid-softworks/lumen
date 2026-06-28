//! Smoke tests for the language core. These are the fast inner loop while growing the engine; the
//! broad conformance signal comes from `crates/test262-runner`.

use crate::{Completion, Engine};

fn run(src: &str) -> String {
    match Engine::new().eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

fn throws(src: &str) -> String {
    match Engine::new().eval(src, false).expect("parse") {
        Completion::Value(v) => panic!("expected throw, got {v}"),
        Completion::Throw { name, .. } => name,
    }
}


#[test]
fn arithmetic() {
    assert_eq!(run("1 + 2 * 3"), "7");
    assert_eq!(run("2 ** 10"), "1024");
    assert_eq!(run("7 % 3"), "1");
    assert_eq!(run("'a' + 'b' + 1"), "ab1");
}

#[test]
fn variables_and_scope() {
    assert_eq!(run("let x = 5; { let x = 9; } x"), "5");
    assert_eq!(run("var a = 1; function f(){ a = 2; } f(); a"), "2");
    assert_eq!(run("const o = {a:1}; o.a += 4; o.a"), "5");
}

#[test]
fn closures() {
    assert_eq!(
        run("function adder(n){ return function(x){ return x + n; }; } adder(10)(5)"),
        "15"
    );
    assert_eq!(run("const inc = x => x + 1; inc(inc(0))"), "2");
}

#[test]
fn control_flow() {
    assert_eq!(run("let s = 0; for (let i = 0; i < 5; i++) s += i; s"), "10");
    assert_eq!(run("let s = 0; for (const v of [1,2,3]) s += v; s"), "6");
    assert_eq!(run("let n = 0, i = 0; while (i < 3) { n += i; i++; } n"), "3");
    assert_eq!(run("function f(x){ if (x>0) return 'pos'; else return 'neg'; } f(-1)"), "neg");
}

#[test]
fn objects_and_prototypes() {
    assert_eq!(run("function P(x){ this.x = x; } P.prototype.get = function(){ return this.x; }; new P(42).get()"), "42");
    assert_eq!(run("const a = [3,1,2]; a.push(4); a.length"), "4");
    assert_eq!(run("[1,2,3].map(x => x*2).join(',')"), "2,4,6");
    assert_eq!(run("[1,2,3,4].filter(x => x%2===0).reduce((a,b)=>a+b,0)"), "6");
}

#[test]
fn errors_have_names() {
    assert_eq!(throws("null.x"), "TypeError");
    assert_eq!(throws("var f = 5; f()"), "TypeError"); // calling a non-function
    assert_eq!(throws("undefinedThing()"), "ReferenceError"); // undeclared variable
    assert_eq!(throws("notDefined"), "ReferenceError");
    assert_eq!(throws("throw new RangeError('bad')"), "RangeError");
    assert_eq!(run("try { null.x } catch (e) { e.name }"), "TypeError");
    assert_eq!(run("try { throw new TypeError('m') } catch (e) { e.message }"), "m");
}

#[test]
fn syntax_error_is_parse_phase() {
    assert!(Engine::new().eval("function (", false).is_err());
    assert!(Engine::new().eval("1 +", false).is_err());
}

#[test]
fn equality_and_coercion() {
    assert_eq!(run("1 == '1'"), "true");
    assert_eq!(run("1 === '1'"), "false");
    assert_eq!(run("null == undefined"), "true");
    assert_eq!(run("NaN === NaN"), "false");
    assert_eq!(run("typeof 1"), "number");
    assert_eq!(run("typeof 'x'"), "string");
    assert_eq!(run("typeof undefinedGlobalThing"), "undefined");
}

#[test]
fn classes_basic() {
    assert_eq!(run("class C {} typeof C"), "function");
    assert_eq!(run("class C { m(){ return 42; } } new C().m()"), "42");
    assert_eq!(run("class C { constructor(x){ this.x = x; } } new C(7).x"), "7");
    assert_eq!(run("class C {} C.name"), "C");
    assert_eq!(run("class C { static s(){ return 9; } } C.s()"), "9");
    assert_eq!(run("class C { #p = 5; get(){ return this.#p; } } new C().get()"), "5");
    assert_eq!(run("class C { f = 3; } new C().f"), "3");
}

#[test]
fn classes_inheritance() {
    let src = "class A { constructor(x){ this.x = x; } hello(){ return 'a' + this.x; } } \
               class B extends A { constructor(x){ super(x); this.y = x*2; } hello(){ return super.hello() + this.y; } } \
               const b = new B(3); b.hello() + ',' + b.y";
    assert_eq!(run(src), "a36,6");
    assert_eq!(run("class A {} class B extends A {} new B() instanceof A"), "true");
    assert_eq!(
        run("class A { m(){return 1;} } class B extends A {} new B().m()"),
        "1"
    );
}

#[test]
fn class_methods_non_enumerable() {
    assert_eq!(run("class C { m(){} } Object.keys(new C()).length"), "0");
    assert_eq!(
        run("class C { get x(){ return 8; } } new C().x"),
        "8"
    );
}

#[test]
fn destructuring() {
    assert_eq!(run("const [a, b] = [1, 2]; a + b"), "3");
    assert_eq!(run("const [a, , c] = [1, 2, 3]; a + c"), "4");
    assert_eq!(run("const [a, ...rest] = [1, 2, 3]; rest.length"), "2");
    assert_eq!(run("const [a = 9] = []; a"), "9");
    assert_eq!(run("const { x, y } = { x: 1, y: 2 }; x + y"), "3");
    assert_eq!(run("const { a: p, b: q = 5 } = { a: 1 }; p + q"), "6");
    assert_eq!(run("const { a, ...rest } = { a: 1, b: 2, c: 3 }; Object.keys(rest).length"), "2");
    assert_eq!(run("function f({ a, b }) { return a + b; } f({ a: 4, b: 5 })"), "9");
    assert_eq!(run("const [[a], { b }] = [[7], { b: 8 }]; a + b"), "15");
    assert_eq!(run("let s = 0; for (const [k, v] of [[1, 2], [3, 4]]) s += k + v; s"), "10");
}

#[test]
fn memory_caps_convert_blowups_to_rangeerror() {
    // Each of these would otherwise allocate unbounded memory; they must throw instead of OOM.
    assert_eq!(throws("new Array(4294967296)"), "RangeError"); // invalid uint32 length
    assert_eq!(throws("[].length = 4294967296"), "RangeError");
    assert_eq!(throws("'x'.repeat(1e9)"), "RangeError");
    assert_eq!(throws("Array(100000000).join(',')"), "RangeError"); // huge length op
    assert_eq!(throws("[...Array(100000000)]"), "RangeError"); // huge spread
    assert_eq!(throws("(123).toFixed(1e9)"), "RangeError");
    assert_eq!(throws("let s='x'; for(;;){ s += s; }"), "RangeError"); // doubling string
    // Truncating a huge sparse length must not loop over the whole range (would hang).
    assert_eq!(run("var a=[1,2,3]; a.length = 1e9; a.length = 1; a.length"), "1");
}

#[test]
fn function_constructor() {
    assert_eq!(run("var f = new Function('a','b','return a+b'); f(2,3)"), "5");
    assert_eq!(run("var f = Function('return 42'); f()"), "42");
    assert_eq!(run("typeof Function"), "function");
    assert_eq!(run("(function(){}) instanceof Function"), "true");
    assert_eq!(run("Function.prototype.call ? 'yes' : 'no'"), "yes");
}

#[test]
fn template_literals() {
    assert_eq!(run("`hello`"), "hello");
    assert_eq!(run("let x = 5; `x is ${x}`"), "x is 5");
    assert_eq!(run("let a=2,b=3; `${a}+${b}=${a+b}`"), "2+3=5");
    assert_eq!(run("`${1}${2}${3}`"), "123");
    assert_eq!(run("let o={n:'q'}; `name: ${o.n}, up: ${o.n.toUpperCase()}`"), "name: q, up: Q");
    assert_eq!(run("`nested ${`a${1}b`} end`"), "nested a1b end");
    assert_eq!(run("`${[1,2,3].map(x=>x*2).join(',')}`"), "2,4,6");
}

#[test]
fn eval_direct_and_indirect() {
    assert_eq!(run("eval('1 + 2 * 3')"), "7");
    assert_eq!(run("eval('var q = 41; q + 1')"), "42");
    assert_eq!(run("var x = 10; eval('x + 5')"), "15"); // direct: sees caller scope
    assert_eq!(run("function f(){ var local = 7; return eval('local * 2'); } f()"), "14");
    assert_eq!(run("eval(42)"), "42"); // non-string returns unchanged
    assert_eq!(run("var e = eval; e('100')"), "100"); // indirect
    assert_eq!(throws("eval('var = =')"), "SyntaxError");
}

#[test]
fn symbols() {
    assert_eq!(run("typeof Symbol()"), "symbol");
    assert_eq!(run("typeof Symbol.iterator"), "symbol");
    assert_eq!(run("Symbol('x') === Symbol('x')"), "false"); // unique
    assert_eq!(run("var s = Symbol('d'); s.description"), "d");
    assert_eq!(run("var s = Symbol(); var o = {}; o[s] = 7; o[s]"), "7");
    assert_eq!(run("var s = Symbol(); var o = {[s]:1, a:2}; Object.keys(o).join(',')"), "a"); // symbol skipped
    assert_eq!(run("var s = Symbol(); var o = {[s]:1}; Object.getOwnPropertySymbols(o).length"), "1");
    assert_eq!(run("Symbol.for('k') === Symbol.for('k')"), "true"); // registry
    assert_eq!(run("String(Symbol('hi'))"), "Symbol(hi)");
    assert_eq!(run("Symbol('z').toString()"), "Symbol(z)");
    assert_eq!(throws("Symbol() + ''"), "TypeError"); // no implicit string coercion
    assert_eq!(throws("+Symbol()"), "TypeError"); // no number coercion
}

#[test]
fn template_with_comments_in_substitution() {
    // Comments inside `${...}` (esp. with apostrophes) must lex cleanly.
    assert_eq!(run("`${ 1 /* a's */ + 2 }`"), "3");
    assert_eq!(run("let x=5; `${ x // it's x\n}`"), "5");
}

#[test]
fn array_methods() {
    assert_eq!(run("[1,2,3,4].find(x=>x>2)"), "3");
    assert_eq!(run("[1,2,3,4].findIndex(x=>x>2)"), "2");
    assert_eq!(run("[1,2,3].some(x=>x>2)"), "true");
    assert_eq!(run("[1,2,3].every(x=>x>0)"), "true");
    assert_eq!(run("[3,1,2].sort().join(',')"), "1,2,3");
    assert_eq!(run("[3,1,2,10].sort((a,b)=>a-b).join(',')"), "1,2,3,10");
    assert_eq!(run("[1,2,3].at(-1)"), "3");
    assert_eq!(run("[1,[2,[3]]].flat(2).join(',')"), "1,2,3");
    assert_eq!(run("[1,2,3].flatMap(x=>[x,x]).join(',')"), "1,1,2,2,3,3");
    assert_eq!(run("var a=[1,2,3,4]; a.splice(1,2,'x'); a.join(',')"), "1,x,4");
    assert_eq!(run("[1,2,3].fill(0,1).join(',')"), "1,0,0");
    assert_eq!(run("Array.from('abc').join(',')"), "a,b,c");
    assert_eq!(run("Array.from([1,2,3], x=>x*2).join(',')"), "2,4,6");
    assert_eq!(run("Array.from({length:3, 0:'a',1:'b',2:'c'}).join(',')"), "a,b,c");
}

#[test]
fn iterator_protocol() {
    assert_eq!(run("[...[1,2,3].keys()].join(',')"), "0,1,2");
    assert_eq!(run("[...[10,20].entries()].map(e=>e.join(':')).join(',')"), "0:10,1:20");
    assert_eq!(run("typeof [][Symbol.iterator]"), "function");
    let custom = "let obj = { [Symbol.iterator]() { let n=0; return { next(){ return n<3 ? {value:n++,done:false} : {value:undefined,done:true}; } }; } };";
    assert_eq!(run(&format!("{custom} let s=0; for (const x of obj) s+=x; s")), "3");
    assert_eq!(run(&format!("{custom} [...obj].join(',')")), "0,1,2");
}

#[test]
fn json_and_reflect() {
    assert_eq!(run("JSON.stringify({a:1,b:[2,3],c:'x'})"), "{\"a\":1,\"b\":[2,3],\"c\":\"x\"}");
    assert_eq!(run("JSON.stringify([1,null,true,'s'])"), "[1,null,true,\"s\"]");
    assert_eq!(run("JSON.stringify({a:undefined,b:function(){},c:1})"), "{\"c\":1}");
    assert_eq!(run("JSON.parse('{\"a\":1,\"b\":[2,3]}').b[1]"), "3");
    assert_eq!(run("JSON.parse('\"hi\\\\n\"').length"), "3");
    assert_eq!(run("JSON.stringify({a:1}, null, 2)"), "{\n  \"a\": 1\n}");
    assert_eq!(throws("var o={}; o.self=o; JSON.stringify(o)"), "TypeError");
    assert_eq!(run("Reflect.has({a:1}, 'a')"), "true");
    assert_eq!(run("Reflect.get({x:7}, 'x')"), "7");
    assert_eq!(run("var o={}; Reflect.set(o,'k',9); o.k"), "9");
    assert_eq!(run("Reflect.ownKeys({a:1,b:2}).join(',')"), "a,b");
    assert_eq!(run("Reflect.apply((a,b)=>a+b, null, [3,4])"), "7");
}

#[test]
fn map_and_set() {
    assert_eq!(run("var m = new Map(); m.set('a',1).set('b',2); m.get('b')"), "2");
    assert_eq!(run("var m = new Map([['x',10],['y',20]]); m.size"), "2");
    assert_eq!(run("var m = new Map(); m.set(1,'a'); m.has(1)"), "true");
    assert_eq!(run("var m = new Map([['a',1]]); m.delete('a'); m.size"), "0");
    assert_eq!(run("var m = new Map([['a',1],['b',2]]); [...m.keys()].join(',')"), "a,b");
    assert_eq!(run("var m = new Map([['a',1],['b',2]]); var s=0; m.forEach(v=>s+=v); s"), "3");
    assert_eq!(run("var s = new Set([1,2,2,3,3,3]); s.size"), "3");
    assert_eq!(run("var s = new Set(); s.add(1).add(1); s.has(1) && s.size===1"), "true");
    assert_eq!(run("[...new Set([3,1,2])].join(',')"), "3,1,2");
    assert_eq!(run("var w = new WeakMap(); var k={}; w.set(k,5); w.get(k)"), "5");
    assert_eq!(throws("new WeakMap().set('str', 1)"), "TypeError"); // non-object key
    assert_eq!(run("NaN === NaN ? 'x' : (new Set([NaN]).has(NaN) ? 'svz' : 'no')"), "svz");
}

#[test]
fn dates() {
    assert_eq!(run("new Date(0).toISOString()"), "1970-01-01T00:00:00.000Z");
    assert_eq!(run("new Date(Date.UTC(2020, 0, 15)).getUTCFullYear()"), "2020");
    assert_eq!(run("new Date(Date.UTC(2020, 5, 15)).getUTCMonth()"), "5");
    assert_eq!(run("Date.parse('2021-06-15T12:30:00.000Z')"), "1623760200000");
    assert_eq!(run("new Date('2000-01-01T00:00:00Z').getTime()"), "946684800000");
    assert_eq!(run("var d = new Date(0); d.setUTCFullYear(1999); d.getUTCFullYear()"), "1999");
    assert_eq!(run("new Date(NaN).toString()"), "Invalid Date");
    assert_eq!(run("JSON.stringify({t: new Date(0)})"), "{\"t\":\"1970-01-01T00:00:00.000Z\"}");
    assert_eq!(run("typeof Date.now()"), "number");
    assert_eq!(run("new Date(Date.UTC(2023,11,25)).getUTCDay()"), "1"); // Monday
}

#[test]
fn typed_arrays() {
    assert_eq!(run("var a = new Int8Array(3); a.length"), "3");
    assert_eq!(run("var a = new Int8Array(3); a[0]=5; a[1]=10; a[0]+a[1]"), "15");
    assert_eq!(run("var a = new Uint8Array([1,2,3]); a.join(',')"), "1,2,3");
    assert_eq!(run("var a = new Int8Array([100]); a[0]=200; a[0]"), "-56"); // wraps i8
    assert_eq!(run("var a = new Uint8ClampedArray([1]); a[0]=300; a[0]"), "255"); // clamps
    assert_eq!(run("new Float64Array([1.5,2.5])[1]"), "2.5");
    assert_eq!(run("Int32Array.BYTES_PER_ELEMENT"), "4");
    assert_eq!(run("var b = new ArrayBuffer(8); b.byteLength"), "8");
    assert_eq!(run("var b = new ArrayBuffer(8); var a = new Int32Array(b); a.length"), "2");
    assert_eq!(run("var a = new Uint8Array([1,2,3,4]); a.subarray(1,3).join(',')"), "2,3");
    assert_eq!(run("var a = new Int16Array(3); a.set([7,8],1); a.join(',')"), "0,7,8");
    assert_eq!(run("new Uint8Array([3,1,2]).map(x=>x*2).join(',')"), "6,2,4");
    assert_eq!(run("ArrayBuffer.isView(new Int8Array(1))"), "true");
    assert_eq!(run("var s=0; new Uint8Array([1,2,3]).forEach(x=>s+=x); s"), "6");
}

#[test]
fn regex() {
    assert_eq!(run("/abc/.test('xabcy')"), "true");
    assert_eq!(run("/^abc$/.test('abc')"), "true");
    assert_eq!(run("/\\d+/.exec('a123b')[0]"), "123");
    assert_eq!(run("/(\\w)(\\w)/.exec('hi')[2]"), "i");
    assert_eq!(run("/a/gi.flags"), "gi");
    assert_eq!(run("/[a-c]+/.exec('xxbcaxx')[0]"), "bca");
    assert_eq!(run("'a1b2c3'.match(/\\d/g).join(',')"), "1,2,3");
    assert_eq!(run("'hello world'.replace(/o/g, '0')"), "hell0 w0rld");
    assert_eq!(run("'2023-06-15'.replace(/(\\d+)-(\\d+)-(\\d+)/, '$3/$2/$1')"), "15/06/2023");
    assert_eq!(run("'a,b;c'.split(/[,;]/).join('|')"), "a|b|c");
    assert_eq!(run("'foobar'.search(/bar/)"), "3");
    assert_eq!(run("/colou?r/.test('color') && /colou?r/.test('colour')"), "true");
    assert_eq!(run("/a(?=b)/.test('ab')"), "true");
    assert_eq!(run("/a(?!b)/.test('ac')"), "true");
    assert_eq!(run("'aaa'.replace(/a/g, x=>x.toUpperCase())"), "AAA");
    assert_eq!(run("/(ab)+/.exec('ababab')[0]"), "ababab");
    assert_eq!(run("/\\bword\\b/.test('a word here')"), "true");
    assert_eq!(run("new RegExp('\\\\d{2,3}').exec('12345')[0]"), "123");
}

#[test]
fn bigint() {
    assert_eq!(run("typeof 10n"), "bigint");
    assert_eq!(run("(10n + 20n).toString()"), "30");
    assert_eq!(run("(2n ** 10n).toString()"), "1024");
    assert_eq!(run("10n === 10n"), "true");
    assert_eq!(run("10n == 10"), "true");
    assert_eq!(run("10n < 20"), "true");
    assert_eq!(run("BigInt(42).toString()"), "42");
    assert_eq!(run("BigInt('100') + 1n === 101n"), "true");
    assert_eq!(run("(-5n).toString()"), "-5");
    assert_eq!(run("(255n).toString(16)"), "ff");
    assert_eq!(run("0xffn.toString()"), "255");
    assert_eq!(run("let x = 5n; x++; x.toString()"), "6");
    assert_eq!(throws("1n + 1"), "TypeError"); // mixing
    assert_eq!(throws("+1n"), "TypeError"); // unary plus on BigInt
    assert_eq!(run("Number(123n)"), "123"); // explicit conversion ok
    assert_eq!(run("String(99n)"), "99");
}

#[test]
fn proxy() {
    assert_eq!(run("var p = new Proxy({a:1}, {}); p.a"), "1"); // forward get
    assert_eq!(run("var p = new Proxy({}, { get(t,k){ return 'X'+k; } }); p.foo"), "Xfoo");
    assert_eq!(run("var t={}; var p = new Proxy(t, { set(o,k,v){ o[k]=v*2; return true; } }); p.x=5; t.x"), "10");
    assert_eq!(run("var p = new Proxy({}, { has(){ return true; } }); 'anything' in p"), "true");
    assert_eq!(run("var p = new Proxy(function(a,b){return a+b;}, {}); p(2,3)"), "5"); // forward apply
    assert_eq!(run("var p = new Proxy(()=>0, { apply(t,th,args){ return args[0]*10; } }); p(7)"), "70");
    assert_eq!(run("var p = new Proxy(function(){ this.v=1; }, {}); new p().v"), "1"); // forward construct
}

#[test]
fn promises() {
    // Microtasks drain at the end of each eval, so a follow-up eval observes the settled state.
    fn after(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        e.eval(setup, false).expect("setup");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    assert_eq!(after("var r=0; Promise.resolve(5).then(v=>v*2).then(v=>{r=v;});", "r"), "10");
    assert_eq!(after("var r; Promise.reject('e').catch(e=>{r='caught:'+e;});", "r"), "caught:e");
    assert_eq!(after("var r; new Promise(res=>res(7)).then(v=>{r=v;});", "r"), "7");
    assert_eq!(
        after("var r; Promise.all([Promise.resolve(1), Promise.resolve(2), 3]).then(a=>{r=a.join(',');});", "r"),
        "1,2,3"
    );
    assert_eq!(
        after("var r; Promise.race([Promise.resolve('fast'), new Promise(()=>{})]).then(v=>{r=v;});", "r"),
        "fast"
    );
    // ordering: synchronous code runs before queued reactions
    assert_eq!(after("var log=[]; Promise.resolve(1).then(v=>log.push(v)); log.push(0);", "log.join(',')"), "0,1");
    assert_eq!(run("typeof Promise.resolve().then"), "function");
}

#[test]
fn generators() {
    assert_eq!(run("function* g(){ yield 1; yield 2; yield 3; } [...g()].join(',')"), "1,2,3");
    assert_eq!(run("function* g(){ yield 1; yield 2; } var it = g(); it.next().value + ',' + it.next().value"), "1,2");
    assert_eq!(run("function* g(){ yield 1; } var it=g(); it.next(); it.next().done"), "true");
    assert_eq!(run("function* g(){ for (let i=0;i<3;i++) yield i*i; } [...g()].join(',')"), "0,1,4");
    assert_eq!(run("function* g(){ yield* [1,2]; yield 3; } [...g()].join(',')"), "1,2,3");
    assert_eq!(run("function* g(){ yield 1; return 99; } var it=g(); it.next(); var r=it.next(); r.value+':'+r.done"), "99:true");
    assert_eq!(run("let s=0; function* g(){ yield 10; yield 20; } for (const x of g()) s+=x; s"), "30");
    assert_eq!(run("class C { *items(){ yield 'a'; yield 'b'; } } [...new C().items()].join(',')"), "a,b");
}

#[test]
fn async_functions() {
    fn after(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        e.eval(setup, false).expect("setup");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    assert_eq!(run("async function f(){ return 5; } typeof f().then"), "function"); // returns a promise
    assert_eq!(after("var r; async function f(){ return 7; } f().then(v=>{r=v;});", "r"), "7");
    assert_eq!(after("var r; async function f(){ return await Promise.resolve(9); } f().then(v=>{r=v;});", "r"), "9");
    assert_eq!(after("var r; async function f(){ try { await Promise.reject('e'); } catch(x){ return 'caught'; } } f().then(v=>{r=v;});", "r"), "caught");
}

#[test]
fn strict_mode_assignment() {
    assert_eq!(throws("'use strict'; undeclaredStrict = 1;"), "ReferenceError");
}

#[test]
fn strict_var_hoisting_in_functions() {
    // `var` inside a function must be hoisted into the function scope, including strict mode (where
    // assignment to an undeclared name would otherwise throw). Regression: hoist was once skipped.
    assert_eq!(run("'use strict'; function f(){ var y = 5; return y; } f()"), "5");
    assert_eq!(run("'use strict'; function f(o){ var label = o && o.x || 'd'; return label; } f()"), "d");
    assert_eq!(run("function f(){ if (true) { var z = 7; } return z; } f()"), "7");
    assert_eq!(run("'use strict'; (function(){ var a; a = 3; return a; })()"), "3");
}

#[test]
fn gc_reclaims_cycles() {
    // Each iteration creates an unreachable reference cycle (o <-> a). Reference counting alone
    // never frees these; the cycle collector must, or live objects would climb without bound.
    let mut e = Engine::new();
    match e
        .eval("var k=0; for (var i=0;i<300000;i++){ var o={}; var a=[o]; o.self=o; o.a=a; k++; } k", false)
        .expect("parse")
    {
        Completion::Value(v) => assert_eq!(v, "300000"),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
    // ~600k cyclic objects were created; after collection only a handful are still reachable.
    let live = crate::value::live_objects();
    assert!(live < 500_000, "live objects after GC loop too high: {live}");
}

#[test]
fn gc_keeps_reachable_cycles() {
    // A cycle still reachable from a live binding must survive collection unscathed.
    assert_eq!(
        run("var o={}; o.self=o; var a=[o]; o.a=a; for(var i=0;i<250000;i++){var t={};t.t=t;} o.a[0].self===o"),
        "true"
    );
}





















#[test]
fn unicode_ident_escapes() {
    assert_eq!(run("var \\u0061 = 5; a"), "5");
    assert_eq!(run("var a\\u0062c = 7; abc"), "7");
    assert_eq!(run("var \\u{61}\\u{62} = 9; ab"), "9");
    assert_eq!(run("var obj = {}; obj.\\u0078 = 3; obj.x"), "3");
}

#[test]
fn bigint_typed_arrays() {
    assert_eq!(run("var a = new BigInt64Array(3); a[0] = 5n; a[1] = -2n; a[0] + a[1]"), "3");
    assert_eq!(run("typeof BigInt64Array"), "function");
    assert_eq!(run("var a = new BigUint64Array([1n, 2n, 3n]); a.length"), "3");
    assert_eq!(run("var a = new BigInt64Array([10n]); typeof a[0]"), "bigint");
    assert_eq!(run("var a = new BigUint64Array(1); a[0] = -1n; a[0]"), "18446744073709551615");
    assert_eq!(run("new BigInt64Array(2).BYTES_PER_ELEMENT"), "8");
}

#[test]
fn with_statement() {
    assert_eq!(run("var o={a:10}; with(o){ a; }"), "10");
    assert_eq!(run("function f(){ var o={a:1}; with(o){ return a; } } f()"), "1");
    assert_eq!(run("var o={x:1}; with(o){ x = 5; } o.x"), "5");
    assert_eq!(run("var a=99; var o={a:1}; with(o){ a; }"), "1");      // object shadows outer
    assert_eq!(run("var a=99; var o={b:1}; with(o){ a; }"), "99");     // falls through to outer
    // `with` in strict mode is a parse-phase SyntaxError.
    assert!(Engine::new().eval("'use strict'; with({}){}", false).is_err());
}


#[test]
fn primitive_wrappers() {
    assert_eq!(run("typeof new Number(5)"), "object");
    assert_eq!(run("typeof Object(5)"), "object");
    assert_eq!(run("typeof new Boolean(true)"), "object");
    assert_eq!(run("typeof new String('x')"), "object");
    assert_eq!(run("typeof Object('s')"), "object");
    assert_eq!(run("new Number(5) + 1"), "6");        // valueOf via this_number
    assert_eq!(run("new String('abc').length"), "3");
    assert_eq!(run("new String('abc')[1]"), "b");
    assert_eq!(run("new String('hi').toUpperCase()"), "HI");
    assert_eq!(run("new Boolean(false).valueOf()"), "false");
    assert_eq!(run("var o=new Number(7); o instanceof Number"), "true");
    assert_eq!(run("typeof Number(5)"), "number");    // call (no new) stays primitive
    assert_eq!(throws("new Symbol()"), "TypeError");
    assert_eq!(throws("new BigInt(1)"), "TypeError");
}

#[test]
fn host_262() {
    assert_eq!(run("typeof $262"), "object");
    assert_eq!(run("$262.global === globalThis"), "true");
    assert_eq!(run("$262.evalScript('1+2')"), "3");
    assert_eq!(run("typeof $262.gc"), "function");
}

#[test]
fn temporal_basics() {
    assert_eq!(run("typeof Temporal"), "object");
    assert_eq!(run("new Temporal.PlainDate(2024,2,29).toString()"), "2024-02-29");
    assert_eq!(run("Temporal.PlainDate.from('2021-07-15').month"), "7");
    assert_eq!(run("new Temporal.PlainDate(2024,1,1).dayOfWeek"), "1"); // Mon
    assert_eq!(run("new Temporal.PlainDate(2024,2,1).daysInMonth"), "29");
    assert_eq!(run("new Temporal.PlainDate(2023,2,1).inLeapYear"), "false");
    assert_eq!(run("new Temporal.PlainDate(2021,1,1).add({days:40}).toString()"), "2021-02-10");
    assert_eq!(run("new Temporal.PlainDate(2021,3,31).add({months:1}).toString()"), "2021-04-30");
    assert_eq!(run("Temporal.PlainDate.compare('2020-01-01','2021-01-01')"), "-1");
    assert_eq!(run("new Temporal.PlainTime(13,5).toString()"), "13:05:00");
    assert_eq!(run("Temporal.Duration.from('P1Y2M3DT4H5M6S').toString()"), "P1Y2M3DT4H5M6S");
    assert_eq!(run("Temporal.Duration.from({hours:1}).negated().hours"), "-1");
    assert_eq!(run("new Temporal.PlainDateTime(2021,7,15,10,30).toString()"), "2021-07-15T10:30:00");
    assert_eq!(run("Temporal.PlainYearMonth.from('2021-07').toString()"), "2021-07");
    assert_eq!(run("Temporal.Instant.fromEpochMilliseconds(0).epochNanoseconds"), "0");
    assert_eq!(throws("Temporal.PlainDate(2020,1,1)"), "TypeError"); // requires new
    assert_eq!(throws("new Temporal.PlainDate(2020,13,1)"), "RangeError");
}

#[test]
fn temporal_until_since() {
    assert_eq!(run("Temporal.PlainDate.from('2021-01-01').until('2021-02-10').days"), "40");
    assert_eq!(run("Temporal.PlainDate.from('2020-01-01').until('2022-03-01',{largestUnit:'year'}).years"), "2");
    assert_eq!(run("Temporal.PlainDate.from('2021-02-10').since('2021-01-01').days"), "40");
    assert_eq!(run("Temporal.PlainTime.from('10:00').until('12:30').hours"), "2");
    assert_eq!(run("Temporal.PlainTime.from('10:00').until('12:30').minutes"), "30");
    assert_eq!(run("Temporal.Instant.fromEpochMilliseconds(0).until(Temporal.Instant.fromEpochMilliseconds(5000)).seconds"), "5");
}

#[test]
fn temporal_zoned() {
    assert_eq!(run("typeof Temporal.ZonedDateTime"), "function");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, 'UTC').year"), "1970");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, 'UTC').epochNanoseconds"), "0");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, 'UTC').toPlainDate().toString()"), "1970-01-01");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, '+05:00').hour"), "5");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, 'UTC').offset"), "+00:00");
    assert_eq!(run("new Temporal.ZonedDateTime(3600000000000n,'UTC').toInstant().epochMilliseconds"), "3600000");
}


#[test]
fn collection_brand_check() {
    assert_eq!(run("var m=new Map(); m.set('a',1); m.get('a')"), "1");   // still works
    assert_eq!(run("new Set([1,2,2]).size"), "2");
    assert_eq!(throws("Map.prototype.get.call({}, 1)"), "TypeError");
    assert_eq!(throws("Set.prototype.add.call([], 1)"), "TypeError");
    assert_eq!(throws("Map.prototype.has.call(5, 1)"), "TypeError");
}

#[test]
fn to_string_tag() {
    assert_eq!(run("Object.prototype.toString.call([])"), "[object Array]");
    assert_eq!(run("Object.prototype.toString.call(null)"), "[object Null]");
    assert_eq!(run("Object.prototype.toString.call(undefined)"), "[object Undefined]");
    assert_eq!(run("Object.prototype.toString.call(function(){})"), "[object Function]");
    assert_eq!(run("Object.prototype.toString.call(new Date())"), "[object Date]");
    assert_eq!(run("Object.prototype.toString.call(/x/)"), "[object RegExp]");
    assert_eq!(run("Object.prototype.toString.call(5)"), "[object Number]");
    assert_eq!(run("Object.prototype.toString.call(new Temporal.PlainDate(2021,1,1))"), "[object Temporal.PlainDate]");
    assert_eq!(run("Object.prototype.toString.call({[Symbol.toStringTag]:'Foo'})"), "[object Foo]");
}

#[test]
fn temporal_tostring_options() {
    assert_eq!(run("new Temporal.PlainTime(1,2,3,456).toString({smallestUnit:'minute'})"), "01:02");
    assert_eq!(run("new Temporal.PlainTime(1,2,3).toString({fractionalSecondDigits:2})"), "01:02:03.00");
    assert_eq!(run("new Temporal.PlainTime(1,2,3,456).toString({fractionalSecondDigits:3})"), "01:02:03.456");
    assert_eq!(run("new Temporal.PlainDate(2021,7,15).toString({calendarName:'always'})"), "2021-07-15[u-ca=iso8601]");
    assert_eq!(run("new Temporal.PlainDate(2021,7,15).toString()"), "2021-07-15");
}

#[test]
fn temporal_duration_round_relative() {
    // P1Y rounded to months relative to 2021-01-01 = 12 months.
    assert_eq!(run("Temporal.Duration.from({years:1}).round({largestUnit:'month', relativeTo:'2021-01-01'}).months"), "12");
    assert_eq!(run("Temporal.Duration.from({months:13}).round({largestUnit:'year', relativeTo:'2021-01-01'}).years"), "1");
    assert_eq!(run("Temporal.Duration.from({days:40}).round({largestUnit:'month', relativeTo:'2021-01-01'}).months"), "1");
}

#[test]
fn temporal_named_timezones() {
    // Fixed-offset named zones.
    assert_eq!(run("new Temporal.ZonedDateTime(0n,'Asia/Kolkata').toPlainTime().toString()"), "05:30:00");
    assert_eq!(run("new Temporal.ZonedDateTime(0n,'Asia/Tokyo').hour"), "9");
    assert_eq!(run("new Temporal.ZonedDateTime(0n,'Asia/Katmandu').minute"), "45");
    // DST: 2021-07-01 is summer -> America/New_York is EDT (-4); winter -> EST (-5).
    assert_eq!(run("Temporal.ZonedDateTime.from('2021-07-01T12:00-04:00[America/New_York]').offset"), "-04:00");
    assert_eq!(run("Temporal.ZonedDateTime.from('2021-01-01T12:00-05:00[America/New_York]').offset"), "-05:00");
    assert_eq!(run("new Temporal.ZonedDateTime(0n,'Africa/Abidjan').offset"), "+00:00");
}


#[test]
fn atomics_basic() {
    assert_eq!(run("typeof Atomics"), "object");
    assert_eq!(run("var a=new Int32Array(new SharedArrayBuffer(16)); Atomics.store(a,0,5); Atomics.load(a,0)"), "5");
    assert_eq!(run("var a=new Int32Array(4); Atomics.add(a,0,3); Atomics.add(a,0,4)"), "3"); // returns old
    assert_eq!(run("var a=new Int32Array(4); Atomics.add(a,0,3); Atomics.add(a,0,4); a[0]"), "7");
    assert_eq!(run("var a=new Int32Array(4); a[0]=8; Atomics.and(a,0,5); a[0]"), "0");
    assert_eq!(run("var a=new Int32Array(4); a[0]=1; Atomics.compareExchange(a,0,1,9); a[0]"), "9");
    assert_eq!(run("Atomics.isLockFree(4)"), "true");
    assert_eq!(run("var a=new BigInt64Array(2); Atomics.store(a,0,7n); Atomics.load(a,0)"), "7");
    assert_eq!(throws("Atomics.add(new Float64Array(2),0,1)"), "TypeError");
    assert_eq!(throws("Atomics.add([],0,1)"), "TypeError");
}


#[test]
fn array_bycopy_groupby() {
    assert_eq!(run("[3,1,2].toReversed().join(',')"), "2,1,3");
    assert_eq!(run("[3,1,2].toSorted().join(',')"), "1,2,3");
    assert_eq!(run("var a=[1,2,3]; a.with(1,9).join(',')+'|'+a.join(',')"), "1,9,3|1,2,3");
    assert_eq!(run("[1,2,3,4].toSpliced(1,2,'a').join(',')"), "1,a,4");
    assert_eq!(run("var g=Object.groupBy([1,2,3,4],x=>x%2?'odd':'even'); g.odd.join(',')+'|'+g.even.join(',')"), "1,3|2,4");
    assert_eq!(run("var r=Promise.withResolvers(); typeof r.promise+typeof r.resolve+typeof r.reject"), "objectfunctionfunction");
}

#[test]
fn resizable_arraybuffer() {
    assert_eq!(run("new ArrayBuffer(8).resizable"), "false");
    assert_eq!(run("new ArrayBuffer(8, {maxByteLength:16}).resizable"), "true");
    assert_eq!(run("new ArrayBuffer(8, {maxByteLength:16}).maxByteLength"), "16");
    assert_eq!(run("var b=new ArrayBuffer(4,{maxByteLength:16}); b.resize(12); b.byteLength"), "12");
    assert_eq!(throws("new ArrayBuffer(4).resize(8)"), "TypeError"); // not resizable
    assert_eq!(throws("new ArrayBuffer(4,{maxByteLength:8}).resize(16)"), "RangeError");
    assert_eq!(run("var b=new ArrayBuffer(4); var c=b.transfer(); b.detached+','+c.byteLength"), "true,4");
}

#[test]
fn misc_globals() {
    assert_eq!(run("Object.hasOwn({a:1},'a')"), "true");
    assert_eq!(run("Object.hasOwn({a:1},'b')"), "false");
    assert_eq!(run("Number.parseInt('42px')"), "42");
    assert_eq!(run("Number.parseInt === parseInt"), "true");
    assert_eq!(run("'abc'.isWellFormed()"), "true");
    assert_eq!(run("var o={}; new WeakRef(o).deref()===o"), "true");
    assert_eq!(run("typeof new FinalizationRegistry(()=>{})"), "object");
    assert_eq!(throws("new WeakRef(5)"), "TypeError");
}

#[test]
fn destructuring_assignment() {
    assert_eq!(run("var a,b; [a,b]=[1,2]; a+','+b"), "1,2");
    assert_eq!(run("var a,b; ({a,b}={a:3,b:4}); a+','+b"), "3,4");
    assert_eq!(run("var a,r; [a,...r]=[1,2,3]; a+'/'+r.join(',')"), "1/2,3");
    assert_eq!(run("var o={}; [o.x,o.y]=[5,6]; o.x+','+o.y"), "5,6");
    assert_eq!(run("var a=9; [a=7]=[]; a"), "7");
    assert_eq!(run("var a,b; ({x:a,y:b}={x:1,y:2}); a+','+b"), "1,2");
    assert_eq!(run("var a,rest; ({a,...rest}={a:1,b:2,c:3}); a+'/'+Object.keys(rest).join(',')"), "1/b,c");
    assert_eq!(run("var a,b; [a,,b]=[1,2,3]; a+','+b"), "1,3");
    assert_eq!(run("var a,b; [[a],{x:b}]=[[7],{x:8}]; a+','+b"), "7,8");
}

#[test]
fn object_literal_methods() {
    assert_eq!(run("({*g(){yield 1; yield 2}}).g().next().value"), "1");
    assert_eq!(run("[...({*g(){yield 1;yield 2}}).g()].join(',')"), "1,2");
    assert_eq!(run("({async m(){return 5}}).m() instanceof Promise"), "true");
    assert_eq!(run("({async(){return 1}}).async()"), "1"); // method named async
    assert_eq!(run("({async:7}).async"), "7"); // property named async
}

#[test]
fn early_errors() {
    // These must be parse-phase SyntaxErrors (Err).
    for src in ["const x", "return 5", "break", "continue", "{break}", "while(0){} break"] {
        assert!(Engine::new().eval(src, false).is_err(), "should reject: {src}");
    }
    // These must still work.
    assert_eq!(run("function f(){return 7} f()"), "7");
    assert_eq!(run("var s=0; for(var i=0;i<3;i++){ if(i==1) continue; s+=i; } s"), "2");
    assert_eq!(run("switch(1){case 1: break; default:} 'ok'"), "ok");
    assert_eq!(run("outer: for(;;){ break outer; } 'ok'"), "ok");
    assert_eq!(run("const y=5; y"), "5");
}

#[test]
fn missing_methods_batch2() {
    assert_eq!(run("Symbol('x').description"), "x");
    assert_eq!(run("typeof Symbol().description"), "undefined");
    assert_eq!(run("Int8Array.of(1,2,3).join(',')"), "1,2,3");
    assert_eq!(run("Int8Array.from([4,5,6],x=>x*2).join(',')"), "8,10,12");
    assert_eq!(run("Uint8Array.from('123').join(',')"), "1,2,3");
    assert_eq!(run("escape('a b+')"), "a%20b+");
    assert_eq!(run("unescape('a%20b%75')"), "a bu");
    assert_eq!(run("'a'.localeCompare('b')"), "-1");
    assert_eq!(run("(255).toLocaleString()"), "255");
}
#[test]
fn ctor_requires_new() {
    for src in ["Map()","Set()","WeakMap()","WeakSet()","Promise(()=>{})","ArrayBuffer(8)","SharedArrayBuffer(8)","Int8Array(4)","Float64Array(2)","DataView(new ArrayBuffer(8))","Proxy({},{})"] {
        assert_eq!(throws(src), "TypeError", "should require new: {src}");
    }
    // With new, all still work.
    assert_eq!(run("new Map([[1,2]]).get(1)"), "2");
    assert_eq!(run("new Int8Array(3).length"), "3");
    assert_eq!(run("new DataView(new ArrayBuffer(8)).byteLength"), "8");
    assert_eq!(run("typeof new Promise(()=>{})"), "object");
}
#[test]
fn subclass_state() {
    assert_eq!(run("class M extends Map{}; new M([[1,2]]).get(1)"), "2");
    assert_eq!(run("class S extends Set{}; var s=new S([3,4]); s.has(3)+''+s.size"), "true2");
    assert_eq!(run("class I extends Int8Array{}; var a=new I([5,6,7]); a[1]"), "6");
    assert_eq!(run("class A extends Array{}; new A(1,2,3).length"), "3");
    assert_eq!(throws("Map()"), "TypeError");
    assert_eq!(throws("Int8Array(3)"), "TypeError");
}

#[test]
fn named_evaluation() {
    assert_eq!(run("var f=function(){}; f.name"), "f");
    assert_eq!(run("let g=()=>{}; g.name"), "g");
    assert_eq!(run("var h; h=function(){}; h.name"), "h");
    assert_eq!(run("({m(){}}).m.name"), "m");
    assert_eq!(run("({foo:function(){}}).foo.name"), "foo");
    assert_eq!(run("var C=class{}; C.name"), "C");
    assert_eq!(run("Object.getOwnPropertyDescriptor({get x(){}},'x').get.name"), "get x");
    assert_eq!(run("function named(){}; var x=named; x.name"), "named"); // keeps original
    assert_eq!(run("(function foo(){}).name"), "foo"); // named expr unchanged
}
#[test]
fn label_validation() {
    assert!(Engine::new().eval("break foo;", false).is_err());
    assert!(Engine::new().eval("x: x: 1", false).is_err());
    assert!(Engine::new().eval("foo: for(;;){ continue bar; }", false).is_err());
    assert_eq!(run("var s=0; outer: for(var i=0;i<3;i++){ for(var j=0;j<3;j++){ if(j==1) continue outer; s++; } } s"), "3");
    assert_eq!(run("a: { break a; } 'ok'"), "ok");
    assert_eq!(run("function f(){ l: for(;;) break l; return 1 } f()"), "1");
    assert_eq!(run("x: 1; x: 2; 'ok'"), "ok"); // sequential same label is fine
}
#[test]
fn named_eval_defaults() {
    assert_eq!(run("var {a=function(){}}={}; a.name"), "a");
    assert_eq!(run("var [b=()=>{}]=[]; b.name"), "b");
    assert_eq!(run("function f(c=function(){}){return c.name}; f()"), "c");
    assert_eq!(run("class C{ m=function(){} }; new C().m.name"), "m");
    assert_eq!(run("var d; ({d=class{}}={}); d.name"), "d");
    assert_eq!(run("var e; [e=function(){}]=[]; e.name"), "e");
    assert_eq!(run("var {x=1}={}; x"), "1"); // non-fn default still works
}
#[test]
fn probe21_tmp() {
    // These should be SyntaxErrors.
    for src in ["let x; let x","{ let y; let y }","let a; const a=1","let b; var b","{ let c; function c(){} }","if(true) let z = 1","while(false) const w = 1","for(;;) let q","label: let p = 1","const d=1; let d","function f(){ let e; let e }","try{}catch(e){ let e }"] {
        eprintln!("RD {src:?} => {}", if crate::Engine::new().eval(src,false).is_err(){"SyntaxErr"}else{"ACCEPTED"});
    }
    // These are fine.
    for src in ["let x; { let x }","{let a}{let a}","let m=1; m=2","var n; var n"] {
        eprintln!("RDok {src:?} => {}", match crate::Engine::new().eval(src,false){Ok(_)=>"ok",Err(_)=>"WRONGLY-REJECTED"});
    }
}
#[test]
fn lexical_substatement() {
    for src in ["if(true) let z = 1","while(false) const w = 1","for(;;) let q","label: let p = 1","if(x) class C{}","do let r=1; while(0)"] {
        assert!(Engine::new().eval(src, false).is_err(), "should reject: {src}");
    }
    // allowed
    assert_eq!(run("if(true) var v = 5; v"), "5");
    assert_eq!(run("if(true) function f(){return 1}; f()"), "1");
    assert_eq!(run("if(true){ let b=2; } 'ok'"), "ok");
    assert_eq!(run("for(let i=0;i<2;i++){} 'ok'"), "ok");
}
#[test]
fn dup_lexical() {
    // errors
    for src in ["let x; let x","{ let y; let y }","let a; const a=1","let b; var b","var bb; let bb","let c; function c(){}","const d=1; let d","class E{}; let E","switch(1){case 1: let s; default: let s}","function z(){ let e; let e }"] {
        assert!(Engine::new().eval(src, false).is_err(), "should reject: {src}");
    }
    // allowed (no false positives)
    for src in ["let x; { let x }","{let a}{let a}","var n; var n","let m=1; m=2","function f(){} function f(){}","for(let i=0;i<2;i++){} for(let i=0;i<2;i++){}","if(1){let p}else{let p}","let q; function g(){ let q }","switch(1){case 1:{let s} case 2:{let s}}","try{}catch(x){let y}"] {
        assert!(Engine::new().eval(src, false).is_ok(), "should accept: {src}");
    }
}
#[test]
fn typeof_tdz() {
    assert_eq!(throws("{ typeof q; let q; }"), "ReferenceError");
    assert_eq!(run("typeof undeclaredXYZ"), "undefined");
    assert_eq!(run("{ let a=1; typeof a }"), "number");
}
#[test]
fn tdz_fn_toplevel() {
    assert_eq!(throws("typeof w; let w;"), "ReferenceError");
    assert_eq!(throws("x; let x=1;"), "ReferenceError");
    assert_eq!(throws("(function(){ typeof r; let r; })()"), "ReferenceError");
    assert_eq!(throws("(function(){ return a; let a; })()"), "ReferenceError");
    // valid uses still work
    assert_eq!(run("let p=1; p"), "1");
    assert_eq!(run("const q=2; q+1"), "3");
    assert_eq!(run("function f(){ let m=5; return m; } f()"), "5");
    assert_eq!(run("var g=10; g"), "10");
    assert_eq!(run("let a=1; { let a=2; } a"), "1");
}
#[test]
fn property_order() {
    assert_eq!(run("Object.keys({2:'a',1:'b',x:'c',0:'d'}).join(',')"), "0,1,2,x");
    assert_eq!(run("var o={b:1}; o.a=2; o[5]=3; o[1]=4; Object.keys(o).join(',')"), "1,5,b,a");
    assert_eq!(run("var r=[]; for(var k in {x:1,2:2,1:3}) r.push(k); r.join(',')"), "1,2,x");
    assert_eq!(run("JSON.stringify({2:'a',1:'b',x:'c'})"), "{\"1\":\"b\",\"2\":\"a\",\"x\":\"c\"}");
    assert_eq!(run("Object.values({2:'a',10:'b',1:'c'}).join(',')"), "c,a,b");
    assert_eq!(run("Object.keys({...{b:1,1:2,a:3}}).join(',')"), "1,b,a");
    assert_eq!(run("var o=Object.assign({},{c:1,1:2,a:3}); Object.keys(o).join(',')"), "1,c,a");
}
#[test]
fn to_primitive_symbol() {
    assert_eq!(run("var o={[Symbol.toPrimitive](h){return h}}; o + ''"), "default");
    assert_eq!(run("var o={[Symbol.toPrimitive](h){return h}}; String(o)"), "string");
    assert_eq!(run("var o={[Symbol.toPrimitive](){return 5}}; o + 1"), "6");
    assert_eq!(run("var o={[Symbol.toPrimitive](){return 5n}}; o + 1n"), "6");
    assert_eq!(run("var o={[Symbol.toPrimitive](){return 42}}; Number(o)"), "42");
    assert_eq!(run("var o={valueOf(){return 9}}; o + 1"), "10");
    assert_eq!(throws("var o={[Symbol.toPrimitive](){return {}}}; o+1"), "TypeError");
}
#[test]
fn date_toprimitive() {
    assert_eq!(run("typeof (new Date(0) + new Date(0))"), "string");
    assert_eq!(run("(new Date(0))[Symbol.toPrimitive]('number')"), "0");
    assert_eq!(run("typeof (new Date(0))[Symbol.toPrimitive]('string')"), "string");
    assert_eq!(run("var d=new Date(0); (d - 0)"), "0"); // number hint via subtraction
}
#[test]
fn not_a_constructor() {
    for src in ["new (Math.max)()","new (parseInt)()","new (Object.keys)()","new (Array.prototype.map)()","new (Array.from)()","new ([].forEach)()","new (JSON.stringify)()","new (String.prototype.slice)()"] {
        assert_eq!(throws(src), "TypeError", "should reject: {src}");
    }
    // real constructors still work
    assert_eq!(run("new Array(3).length"), "3");
    assert_eq!(run("new Map([[1,2]]).get(1)"), "2");
    assert_eq!(run("typeof new Date(0)"), "object");
    assert_eq!(run("new Number(5).valueOf()"), "5");
    assert_eq!(run("new RegExp('a').source"), "a");
    assert_eq!(run("new Int8Array(2).length"), "2");
    assert_eq!(run("class C{}; typeof new C()"), "object");
    assert_eq!(run("function F(){this.x=1}; new F().x"), "1");
    assert_eq!(run("new Error('m').message"), "m");
}
#[test]
fn array_length_index() {
    assert_eq!(run("var a=[]; a[4294967295]=1; a.length"), "0");
    assert_eq!(run("var a=[]; a[4294967294]=1; a.length"), "4294967295");
    assert_eq!(run("var a=[]; a[5]=1; a.length"), "6");
    assert_eq!(throws("var a=[]; a.length=4294967296"), "RangeError");
    assert_eq!(run("var a=[]; a['foo']=1; a.length"), "0");
    assert_eq!(run("[1,2,3].length"), "3");
    assert_eq!(run("var a=[]; a[4294967295]=1; a[4294967295]"), "1"); // still stored as prop
}
#[test]
fn species_getters() {
    assert_eq!(run("Array[Symbol.species]===Array"), "true");
    assert_eq!(run("Map[Symbol.species]===Map"), "true");
    assert_eq!(run("Set[Symbol.species]===Set"), "true");
    assert_eq!(run("Promise[Symbol.species]===Promise"), "true");
    assert_eq!(run("RegExp[Symbol.species]===RegExp"), "true");
    assert_eq!(run("typeof Object.getOwnPropertyDescriptor(Array,Symbol.species).get"), "function");
}
