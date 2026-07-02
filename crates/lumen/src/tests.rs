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
    assert_eq!(
        run("let s = 0; for (let i = 0; i < 5; i++) s += i; s"),
        "10"
    );
    assert_eq!(run("let s = 0; for (const v of [1,2,3]) s += v; s"), "6");
    assert_eq!(
        run("let n = 0, i = 0; while (i < 3) { n += i; i++; } n"),
        "3"
    );
    assert_eq!(
        run("function f(x){ if (x>0) return 'pos'; else return 'neg'; } f(-1)"),
        "neg"
    );
}

#[test]
fn objects_and_prototypes() {
    assert_eq!(run("function P(x){ this.x = x; } P.prototype.get = function(){ return this.x; }; new P(42).get()"), "42");
    assert_eq!(run("const a = [3,1,2]; a.push(4); a.length"), "4");
    assert_eq!(run("[1,2,3].map(x => x*2).join(',')"), "2,4,6");
    assert_eq!(
        run("[1,2,3,4].filter(x => x%2===0).reduce((a,b)=>a+b,0)"),
        "6"
    );
}

#[test]
fn errors_have_names() {
    assert_eq!(throws("null.x"), "TypeError");
    assert_eq!(throws("var f = 5; f()"), "TypeError"); // calling a non-function
    assert_eq!(throws("undefinedThing()"), "ReferenceError"); // undeclared variable
    assert_eq!(throws("notDefined"), "ReferenceError");
    assert_eq!(throws("throw new RangeError('bad')"), "RangeError");
    assert_eq!(run("try { null.x } catch (e) { e.name }"), "TypeError");
    assert_eq!(
        run("try { throw new TypeError('m') } catch (e) { e.message }"),
        "m"
    );
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
    assert_eq!(
        run("class C { constructor(x){ this.x = x; } } new C(7).x"),
        "7"
    );
    assert_eq!(run("class C {} C.name"), "C");
    assert_eq!(run("class C { static s(){ return 9; } } C.s()"), "9");
    assert_eq!(
        run("class C { #p = 5; get(){ return this.#p; } } new C().get()"),
        "5"
    );
    assert_eq!(run("class C { f = 3; } new C().f"), "3");
}

#[test]
fn classes_inheritance() {
    let src = "class A { constructor(x){ this.x = x; } hello(){ return 'a' + this.x; } } \
               class B extends A { constructor(x){ super(x); this.y = x*2; } hello(){ return super.hello() + this.y; } } \
               const b = new B(3); b.hello() + ',' + b.y";
    assert_eq!(run(src), "a36,6");
    assert_eq!(
        run("class A {} class B extends A {} new B() instanceof A"),
        "true"
    );
    assert_eq!(
        run("class A { m(){return 1;} } class B extends A {} new B().m()"),
        "1"
    );
}

#[test]
fn class_methods_non_enumerable() {
    assert_eq!(run("class C { m(){} } Object.keys(new C()).length"), "0");
    assert_eq!(run("class C { get x(){ return 8; } } new C().x"), "8");
}

#[test]
fn destructuring() {
    assert_eq!(run("const [a, b] = [1, 2]; a + b"), "3");
    assert_eq!(run("const [a, , c] = [1, 2, 3]; a + c"), "4");
    assert_eq!(run("const [a, ...rest] = [1, 2, 3]; rest.length"), "2");
    assert_eq!(run("const [a = 9] = []; a"), "9");
    assert_eq!(run("const { x, y } = { x: 1, y: 2 }; x + y"), "3");
    assert_eq!(run("const { a: p, b: q = 5 } = { a: 1 }; p + q"), "6");
    assert_eq!(
        run("const { a, ...rest } = { a: 1, b: 2, c: 3 }; Object.keys(rest).length"),
        "2"
    );
    assert_eq!(
        run("function f({ a, b }) { return a + b; } f({ a: 4, b: 5 })"),
        "9"
    );
    assert_eq!(run("const [[a], { b }] = [[7], { b: 8 }]; a + b"), "15");
    assert_eq!(
        run("let s = 0; for (const [k, v] of [[1, 2], [3, 4]]) s += k + v; s"),
        "10"
    );
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
    assert_eq!(
        run("var a=[1,2,3]; a.length = 1e9; a.length = 1; a.length"),
        "1"
    );
}

#[test]
fn function_constructor() {
    assert_eq!(
        run("var f = new Function('a','b','return a+b'); f(2,3)"),
        "5"
    );
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
    assert_eq!(
        run("let o={n:'q'}; `name: ${o.n}, up: ${o.n.toUpperCase()}`"),
        "name: q, up: Q"
    );
    assert_eq!(run("`nested ${`a${1}b`} end`"), "nested a1b end");
    assert_eq!(run("`${[1,2,3].map(x=>x*2).join(',')}`"), "2,4,6");
}

#[test]
fn eval_direct_and_indirect() {
    assert_eq!(run("eval('1 + 2 * 3')"), "7");
    assert_eq!(run("eval('var q = 41; q + 1')"), "42");
    assert_eq!(run("var x = 10; eval('x + 5')"), "15"); // direct: sees caller scope
    assert_eq!(
        run("function f(){ var local = 7; return eval('local * 2'); } f()"),
        "14"
    );
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
    assert_eq!(
        run("var s = Symbol(); var o = {[s]:1, a:2}; Object.keys(o).join(',')"),
        "a"
    ); // symbol skipped
    assert_eq!(
        run("var s = Symbol(); var o = {[s]:1}; Object.getOwnPropertySymbols(o).length"),
        "1"
    );
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
    assert_eq!(
        run("var a=[1,2,3,4]; a.splice(1,2,'x'); a.join(',')"),
        "1,x,4"
    );
    assert_eq!(run("[1,2,3].fill(0,1).join(',')"), "1,0,0");
    assert_eq!(run("Array.from('abc').join(',')"), "a,b,c");
    assert_eq!(run("Array.from([1,2,3], x=>x*2).join(',')"), "2,4,6");
    assert_eq!(
        run("Array.from({length:3, 0:'a',1:'b',2:'c'}).join(',')"),
        "a,b,c"
    );
}

#[test]
fn iterator_protocol() {
    assert_eq!(run("[...[1,2,3].keys()].join(',')"), "0,1,2");
    assert_eq!(
        run("[...[10,20].entries()].map(e=>e.join(':')).join(',')"),
        "0:10,1:20"
    );
    assert_eq!(run("typeof [][Symbol.iterator]"), "function");
    let custom = "let obj = { [Symbol.iterator]() { let n=0; return { next(){ return n<3 ? {value:n++,done:false} : {value:undefined,done:true}; } }; } };";
    assert_eq!(
        run(&format!("{custom} let s=0; for (const x of obj) s+=x; s")),
        "3"
    );
    assert_eq!(run(&format!("{custom} [...obj].join(',')")), "0,1,2");
}

#[test]
fn json_and_reflect() {
    assert_eq!(
        run("JSON.stringify({a:1,b:[2,3],c:'x'})"),
        "{\"a\":1,\"b\":[2,3],\"c\":\"x\"}"
    );
    assert_eq!(
        run("JSON.stringify([1,null,true,'s'])"),
        "[1,null,true,\"s\"]"
    );
    assert_eq!(
        run("JSON.stringify({a:undefined,b:function(){},c:1})"),
        "{\"c\":1}"
    );
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
    assert_eq!(
        run("var m = new Map(); m.set('a',1).set('b',2); m.get('b')"),
        "2"
    );
    assert_eq!(run("var m = new Map([['x',10],['y',20]]); m.size"), "2");
    assert_eq!(run("var m = new Map(); m.set(1,'a'); m.has(1)"), "true");
    assert_eq!(
        run("var m = new Map([['a',1]]); m.delete('a'); m.size"),
        "0"
    );
    assert_eq!(
        run("var m = new Map([['a',1],['b',2]]); [...m.keys()].join(',')"),
        "a,b"
    );
    assert_eq!(
        run("var m = new Map([['a',1],['b',2]]); var s=0; m.forEach(v=>s+=v); s"),
        "3"
    );
    assert_eq!(run("var s = new Set([1,2,2,3,3,3]); s.size"), "3");
    assert_eq!(
        run("var s = new Set(); s.add(1).add(1); s.has(1) && s.size===1"),
        "true"
    );
    assert_eq!(run("[...new Set([3,1,2])].join(',')"), "3,1,2");
    assert_eq!(
        run("var w = new WeakMap(); var k={}; w.set(k,5); w.get(k)"),
        "5"
    );
    assert_eq!(throws("new WeakMap().set('str', 1)"), "TypeError"); // non-object key
    assert_eq!(
        run("NaN === NaN ? 'x' : (new Set([NaN]).has(NaN) ? 'svz' : 'no')"),
        "svz"
    );
}

#[test]
fn dates() {
    assert_eq!(run("new Date(0).toISOString()"), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        run("new Date(Date.UTC(2020, 0, 15)).getUTCFullYear()"),
        "2020"
    );
    assert_eq!(run("new Date(Date.UTC(2020, 5, 15)).getUTCMonth()"), "5");
    assert_eq!(
        run("Date.parse('2021-06-15T12:30:00.000Z')"),
        "1623760200000"
    );
    assert_eq!(
        run("new Date('2000-01-01T00:00:00Z').getTime()"),
        "946684800000"
    );
    assert_eq!(
        run("var d = new Date(0); d.setUTCFullYear(1999); d.getUTCFullYear()"),
        "1999"
    );
    assert_eq!(run("new Date(NaN).toString()"), "Invalid Date");
    assert_eq!(
        run("JSON.stringify({t: new Date(0)})"),
        "{\"t\":\"1970-01-01T00:00:00.000Z\"}"
    );
    assert_eq!(run("typeof Date.now()"), "number");
    assert_eq!(run("new Date(Date.UTC(2023,11,25)).getUTCDay()"), "1"); // Monday
}

#[test]
fn typed_arrays() {
    assert_eq!(run("var a = new Int8Array(3); a.length"), "3");
    assert_eq!(
        run("var a = new Int8Array(3); a[0]=5; a[1]=10; a[0]+a[1]"),
        "15"
    );
    assert_eq!(run("var a = new Uint8Array([1,2,3]); a.join(',')"), "1,2,3");
    assert_eq!(run("var a = new Int8Array([100]); a[0]=200; a[0]"), "-56"); // wraps i8
    assert_eq!(
        run("var a = new Uint8ClampedArray([1]); a[0]=300; a[0]"),
        "255"
    ); // clamps
    assert_eq!(run("new Float64Array([1.5,2.5])[1]"), "2.5");
    assert_eq!(run("Int32Array.BYTES_PER_ELEMENT"), "4");
    assert_eq!(run("var b = new ArrayBuffer(8); b.byteLength"), "8");
    assert_eq!(
        run("var b = new ArrayBuffer(8); var a = new Int32Array(b); a.length"),
        "2"
    );
    assert_eq!(
        run("var a = new Uint8Array([1,2,3,4]); a.subarray(1,3).join(',')"),
        "2,3"
    );
    assert_eq!(
        run("var a = new Int16Array(3); a.set([7,8],1); a.join(',')"),
        "0,7,8"
    );
    assert_eq!(
        run("new Uint8Array([3,1,2]).map(x=>x*2).join(',')"),
        "6,2,4"
    );
    assert_eq!(run("ArrayBuffer.isView(new Int8Array(1))"), "true");
    assert_eq!(
        run("var s=0; new Uint8Array([1,2,3]).forEach(x=>s+=x); s"),
        "6"
    );
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
    assert_eq!(
        run("'2023-06-15'.replace(/(\\d+)-(\\d+)-(\\d+)/, '$3/$2/$1')"),
        "15/06/2023"
    );
    assert_eq!(run("'a,b;c'.split(/[,;]/).join('|')"), "a|b|c");
    assert_eq!(run("'foobar'.search(/bar/)"), "3");
    assert_eq!(
        run("/colou?r/.test('color') && /colou?r/.test('colour')"),
        "true"
    );
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
    assert_eq!(
        run("var p = new Proxy({}, { get(t,k){ return 'X'+k; } }); p.foo"),
        "Xfoo"
    );
    assert_eq!(
        run("var t={}; var p = new Proxy(t, { set(o,k,v){ o[k]=v*2; return true; } }); p.x=5; t.x"),
        "10"
    );
    assert_eq!(
        run("var p = new Proxy({}, { has(){ return true; } }); 'anything' in p"),
        "true"
    );
    assert_eq!(
        run("var p = new Proxy(function(a,b){return a+b;}, {}); p(2,3)"),
        "5"
    ); // forward apply
    assert_eq!(
        run("var p = new Proxy(()=>0, { apply(t,th,args){ return args[0]*10; } }); p(7)"),
        "70"
    );
    assert_eq!(
        run("var p = new Proxy(function(){ this.v=1; }, {}); new p().v"),
        "1"
    ); // forward construct
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
    assert_eq!(
        after(
            "var r=0; Promise.resolve(5).then(v=>v*2).then(v=>{r=v;});",
            "r"
        ),
        "10"
    );
    assert_eq!(
        after(
            "var r; Promise.reject('e').catch(e=>{r='caught:'+e;});",
            "r"
        ),
        "caught:e"
    );
    assert_eq!(
        after("var r; new Promise(res=>res(7)).then(v=>{r=v;});", "r"),
        "7"
    );
    assert_eq!(
        after("var r; Promise.all([Promise.resolve(1), Promise.resolve(2), 3]).then(a=>{r=a.join(',');});", "r"),
        "1,2,3"
    );
    assert_eq!(
        after(
            "var r; Promise.race([Promise.resolve('fast'), new Promise(()=>{})]).then(v=>{r=v;});",
            "r"
        ),
        "fast"
    );
    // ordering: synchronous code runs before queued reactions
    assert_eq!(
        after(
            "var log=[]; Promise.resolve(1).then(v=>log.push(v)); log.push(0);",
            "log.join(',')"
        ),
        "0,1"
    );
    assert_eq!(run("typeof Promise.resolve().then"), "function");
}

#[test]
fn generators() {
    assert_eq!(
        run("function* g(){ yield 1; yield 2; yield 3; } [...g()].join(',')"),
        "1,2,3"
    );
    assert_eq!(run("function* g(){ yield 1; yield 2; } var it = g(); it.next().value + ',' + it.next().value"), "1,2");
    assert_eq!(
        run("function* g(){ yield 1; } var it=g(); it.next(); it.next().done"),
        "true"
    );
    assert_eq!(
        run("function* g(){ for (let i=0;i<3;i++) yield i*i; } [...g()].join(',')"),
        "0,1,4"
    );
    assert_eq!(
        run("function* g(){ yield* [1,2]; yield 3; } [...g()].join(',')"),
        "1,2,3"
    );
    assert_eq!(run("function* g(){ yield 1; return 99; } var it=g(); it.next(); var r=it.next(); r.value+':'+r.done"), "99:true");
    assert_eq!(
        run("let s=0; function* g(){ yield 10; yield 20; } for (const x of g()) s+=x; s"),
        "30"
    );
    assert_eq!(
        run("class C { *items(){ yield 'a'; yield 'b'; } } [...new C().items()].join(',')"),
        "a,b"
    );
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
    assert_eq!(
        run("async function f(){ return 5; } typeof f().then"),
        "function"
    ); // returns a promise
    assert_eq!(
        after(
            "var r; async function f(){ return 7; } f().then(v=>{r=v;});",
            "r"
        ),
        "7"
    );
    assert_eq!(
        after(
            "var r; async function f(){ return await Promise.resolve(9); } f().then(v=>{r=v;});",
            "r"
        ),
        "9"
    );
    assert_eq!(after("var r; async function f(){ try { await Promise.reject('e'); } catch(x){ return 'caught'; } } f().then(v=>{r=v;});", "r"), "caught");
}

#[test]
fn strict_mode_assignment() {
    assert_eq!(
        throws("'use strict'; undeclaredStrict = 1;"),
        "ReferenceError"
    );
}

#[test]
fn strict_var_hoisting_in_functions() {
    // `var` inside a function must be hoisted into the function scope, including strict mode (where
    // assignment to an undeclared name would otherwise throw). Regression: hoist was once skipped.
    assert_eq!(
        run("'use strict'; function f(){ var y = 5; return y; } f()"),
        "5"
    );
    assert_eq!(
        run("'use strict'; function f(o){ var label = o && o.x || 'd'; return label; } f()"),
        "d"
    );
    assert_eq!(
        run("function f(){ if (true) { var z = 7; } return z; } f()"),
        "7"
    );
    assert_eq!(
        run("'use strict'; (function(){ var a; a = 3; return a; })()"),
        "3"
    );
}

#[test]
fn gc_reclaims_cycles() {
    // Each iteration creates an unreachable reference cycle (o <-> a). Reference counting alone
    // never frees these; the cycle collector must, or live objects would climb without bound.
    let mut e = Engine::new();
    match e
        .eval(
            "var k=0; for (var i=0;i<300000;i++){ var o={}; var a=[o]; o.self=o; o.a=a; k++; } k",
            false,
        )
        .expect("parse")
    {
        Completion::Value(v) => assert_eq!(v, "300000"),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
    // ~600k cyclic objects were created; after collection only a handful are still reachable.
    let live = crate::value::live_objects();
    assert!(
        live < 500_000,
        "live objects after GC loop too high: {live}"
    );
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
    assert_eq!(
        run("var a = new BigInt64Array(3); a[0] = 5n; a[1] = -2n; a[0] + a[1]"),
        "3"
    );
    assert_eq!(run("typeof BigInt64Array"), "function");
    assert_eq!(
        run("var a = new BigUint64Array([1n, 2n, 3n]); a.length"),
        "3"
    );
    assert_eq!(
        run("var a = new BigInt64Array([10n]); typeof a[0]"),
        "bigint"
    );
    assert_eq!(
        run("var a = new BigUint64Array(1); a[0] = -1n; a[0]"),
        "18446744073709551615"
    );
    assert_eq!(run("new BigInt64Array(2).BYTES_PER_ELEMENT"), "8");
}

#[test]
fn with_statement() {
    assert_eq!(run("var o={a:10}; with(o){ a; }"), "10");
    assert_eq!(
        run("function f(){ var o={a:1}; with(o){ return a; } } f()"),
        "1"
    );
    assert_eq!(run("var o={x:1}; with(o){ x = 5; } o.x"), "5");
    assert_eq!(run("var a=99; var o={a:1}; with(o){ a; }"), "1"); // object shadows outer
    assert_eq!(run("var a=99; var o={b:1}; with(o){ a; }"), "99"); // falls through to outer
                                                                   // `with` in strict mode is a parse-phase SyntaxError.
    assert!(Engine::new()
        .eval("'use strict'; with({}){}", false)
        .is_err());
}

#[test]
fn primitive_wrappers() {
    assert_eq!(run("typeof new Number(5)"), "object");
    assert_eq!(run("typeof Object(5)"), "object");
    assert_eq!(run("typeof new Boolean(true)"), "object");
    assert_eq!(run("typeof new String('x')"), "object");
    assert_eq!(run("typeof Object('s')"), "object");
    assert_eq!(run("new Number(5) + 1"), "6"); // valueOf via this_number
    assert_eq!(run("new String('abc').length"), "3");
    assert_eq!(run("new String('abc')[1]"), "b");
    assert_eq!(run("new String('hi').toUpperCase()"), "HI");
    assert_eq!(run("new Boolean(false).valueOf()"), "false");
    assert_eq!(run("var o=new Number(7); o instanceof Number"), "true");
    assert_eq!(run("typeof Number(5)"), "number"); // call (no new) stays primitive
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
    assert_eq!(
        run("new Temporal.PlainDate(2024,2,29).toString()"),
        "2024-02-29"
    );
    assert_eq!(run("Temporal.PlainDate.from('2021-07-15').month"), "7");
    assert_eq!(run("new Temporal.PlainDate(2024,1,1).dayOfWeek"), "1"); // Mon
    assert_eq!(run("new Temporal.PlainDate(2024,2,1).daysInMonth"), "29");
    assert_eq!(run("new Temporal.PlainDate(2023,2,1).inLeapYear"), "false");
    assert_eq!(
        run("new Temporal.PlainDate(2021,1,1).add({days:40}).toString()"),
        "2021-02-10"
    );
    assert_eq!(
        run("new Temporal.PlainDate(2021,3,31).add({months:1}).toString()"),
        "2021-04-30"
    );
    assert_eq!(
        run("Temporal.PlainDate.compare('2020-01-01','2021-01-01')"),
        "-1"
    );
    assert_eq!(run("new Temporal.PlainTime(13,5).toString()"), "13:05:00");
    assert_eq!(
        run("Temporal.Duration.from('P1Y2M3DT4H5M6S').toString()"),
        "P1Y2M3DT4H5M6S"
    );
    assert_eq!(
        run("Temporal.Duration.from({hours:1}).negated().hours"),
        "-1"
    );
    assert_eq!(
        run("new Temporal.PlainDateTime(2021,7,15,10,30).toString()"),
        "2021-07-15T10:30:00"
    );
    assert_eq!(
        run("Temporal.PlainYearMonth.from('2021-07').toString()"),
        "2021-07"
    );
    assert_eq!(
        run("Temporal.Instant.fromEpochMilliseconds(0).epochNanoseconds"),
        "0"
    );
    assert_eq!(throws("Temporal.PlainDate(2020,1,1)"), "TypeError"); // requires new
    assert_eq!(throws("new Temporal.PlainDate(2020,13,1)"), "RangeError");
}

#[test]
fn temporal_until_since() {
    assert_eq!(
        run("Temporal.PlainDate.from('2021-01-01').until('2021-02-10').days"),
        "40"
    );
    assert_eq!(
        run("Temporal.PlainDate.from('2020-01-01').until('2022-03-01',{largestUnit:'year'}).years"),
        "2"
    );
    assert_eq!(
        run("Temporal.PlainDate.from('2021-02-10').since('2021-01-01').days"),
        "40"
    );
    assert_eq!(
        run("Temporal.PlainTime.from('10:00').until('12:30').hours"),
        "2"
    );
    assert_eq!(
        run("Temporal.PlainTime.from('10:00').until('12:30').minutes"),
        "30"
    );
    assert_eq!(run("Temporal.Instant.fromEpochMilliseconds(0).until(Temporal.Instant.fromEpochMilliseconds(5000)).seconds"), "5");
}

#[test]
fn temporal_zoned() {
    assert_eq!(run("typeof Temporal.ZonedDateTime"), "function");
    assert_eq!(run("new Temporal.ZonedDateTime(0n, 'UTC').year"), "1970");
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n, 'UTC').epochNanoseconds"),
        "0"
    );
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n, 'UTC').toPlainDate().toString()"),
        "1970-01-01"
    );
    assert_eq!(run("new Temporal.ZonedDateTime(0n, '+05:00').hour"), "5");
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n, 'UTC').offset"),
        "+00:00"
    );
    assert_eq!(
        run("new Temporal.ZonedDateTime(3600000000000n,'UTC').toInstant().epochMilliseconds"),
        "3600000"
    );
}

#[test]
fn collection_brand_check() {
    assert_eq!(run("var m=new Map(); m.set('a',1); m.get('a')"), "1"); // still works
    assert_eq!(run("new Set([1,2,2]).size"), "2");
    assert_eq!(throws("Map.prototype.get.call({}, 1)"), "TypeError");
    assert_eq!(throws("Set.prototype.add.call([], 1)"), "TypeError");
    assert_eq!(throws("Map.prototype.has.call(5, 1)"), "TypeError");
}

#[test]
fn to_string_tag() {
    assert_eq!(run("Object.prototype.toString.call([])"), "[object Array]");
    assert_eq!(run("Object.prototype.toString.call(null)"), "[object Null]");
    assert_eq!(
        run("Object.prototype.toString.call(undefined)"),
        "[object Undefined]"
    );
    assert_eq!(
        run("Object.prototype.toString.call(function(){})"),
        "[object Function]"
    );
    assert_eq!(
        run("Object.prototype.toString.call(new Date())"),
        "[object Date]"
    );
    assert_eq!(
        run("Object.prototype.toString.call(/x/)"),
        "[object RegExp]"
    );
    assert_eq!(run("Object.prototype.toString.call(5)"), "[object Number]");
    assert_eq!(
        run("Object.prototype.toString.call(new Temporal.PlainDate(2021,1,1))"),
        "[object Temporal.PlainDate]"
    );
    assert_eq!(
        run("Object.prototype.toString.call({[Symbol.toStringTag]:'Foo'})"),
        "[object Foo]"
    );
}

#[test]
fn temporal_tostring_options() {
    assert_eq!(
        run("new Temporal.PlainTime(1,2,3,456).toString({smallestUnit:'minute'})"),
        "01:02"
    );
    assert_eq!(
        run("new Temporal.PlainTime(1,2,3).toString({fractionalSecondDigits:2})"),
        "01:02:03.00"
    );
    assert_eq!(
        run("new Temporal.PlainTime(1,2,3,456).toString({fractionalSecondDigits:3})"),
        "01:02:03.456"
    );
    assert_eq!(
        run("new Temporal.PlainDate(2021,7,15).toString({calendarName:'always'})"),
        "2021-07-15[u-ca=iso8601]"
    );
    assert_eq!(
        run("new Temporal.PlainDate(2021,7,15).toString()"),
        "2021-07-15"
    );
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
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n,'Asia/Kolkata').toPlainTime().toString()"),
        "05:30:00"
    );
    assert_eq!(run("new Temporal.ZonedDateTime(0n,'Asia/Tokyo').hour"), "9");
    // Nepal is +05:45, but only since 1986-01-01 (it was +05:30 before, incl. at epoch 0), so use a
    // 2000-01-01T00:00:00Z instant to exercise the quarter-hour offset.
    assert_eq!(
        run("new Temporal.ZonedDateTime(946684800000000000n,'Asia/Katmandu').minute"),
        "45"
    );
    // DST: 2021-07-01 is summer -> America/New_York is EDT (-4); winter -> EST (-5).
    assert_eq!(
        run("Temporal.ZonedDateTime.from('2021-07-01T12:00-04:00[America/New_York]').offset"),
        "-04:00"
    );
    assert_eq!(
        run("Temporal.ZonedDateTime.from('2021-01-01T12:00-05:00[America/New_York]').offset"),
        "-05:00"
    );
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n,'Africa/Abidjan').offset"),
        "+00:00"
    );
}

#[test]
fn atomics_basic() {
    assert_eq!(run("typeof Atomics"), "object");
    assert_eq!(run("var a=new Int32Array(new SharedArrayBuffer(16)); Atomics.store(a,0,5); Atomics.load(a,0)"), "5");
    assert_eq!(
        run("var a=new Int32Array(4); Atomics.add(a,0,3); Atomics.add(a,0,4)"),
        "3"
    ); // returns old
    assert_eq!(
        run("var a=new Int32Array(4); Atomics.add(a,0,3); Atomics.add(a,0,4); a[0]"),
        "7"
    );
    assert_eq!(
        run("var a=new Int32Array(4); a[0]=8; Atomics.and(a,0,5); a[0]"),
        "0"
    );
    assert_eq!(
        run("var a=new Int32Array(4); a[0]=1; Atomics.compareExchange(a,0,1,9); a[0]"),
        "9"
    );
    assert_eq!(run("Atomics.isLockFree(4)"), "true");
    assert_eq!(
        run("var a=new BigInt64Array(2); Atomics.store(a,0,7n); Atomics.load(a,0)"),
        "7"
    );
    assert_eq!(throws("Atomics.add(new Float64Array(2),0,1)"), "TypeError");
    assert_eq!(throws("Atomics.add([],0,1)"), "TypeError");
}

#[test]
fn array_bycopy_groupby() {
    assert_eq!(run("[3,1,2].toReversed().join(',')"), "2,1,3");
    assert_eq!(run("[3,1,2].toSorted().join(',')"), "1,2,3");
    assert_eq!(
        run("var a=[1,2,3]; a.with(1,9).join(',')+'|'+a.join(',')"),
        "1,9,3|1,2,3"
    );
    assert_eq!(run("[1,2,3,4].toSpliced(1,2,'a').join(',')"), "1,a,4");
    assert_eq!(run("var g=Object.groupBy([1,2,3,4],x=>x%2?'odd':'even'); g.odd.join(',')+'|'+g.even.join(',')"), "1,3|2,4");
    assert_eq!(
        run("var r=Promise.withResolvers(); typeof r.promise+typeof r.resolve+typeof r.reject"),
        "objectfunctionfunction"
    );
}

#[test]
fn resizable_arraybuffer() {
    assert_eq!(run("new ArrayBuffer(8).resizable"), "false");
    assert_eq!(
        run("new ArrayBuffer(8, {maxByteLength:16}).resizable"),
        "true"
    );
    assert_eq!(
        run("new ArrayBuffer(8, {maxByteLength:16}).maxByteLength"),
        "16"
    );
    assert_eq!(
        run("var b=new ArrayBuffer(4,{maxByteLength:16}); b.resize(12); b.byteLength"),
        "12"
    );
    assert_eq!(throws("new ArrayBuffer(4).resize(8)"), "TypeError"); // not resizable
    assert_eq!(
        throws("new ArrayBuffer(4,{maxByteLength:8}).resize(16)"),
        "RangeError"
    );
    assert_eq!(
        run("var b=new ArrayBuffer(4); var c=b.transfer(); b.detached+','+c.byteLength"),
        "true,4"
    );
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
    assert_eq!(
        run("var a,rest; ({a,...rest}={a:1,b:2,c:3}); a+'/'+Object.keys(rest).join(',')"),
        "1/b,c"
    );
    assert_eq!(run("var a,b; [a,,b]=[1,2,3]; a+','+b"), "1,3");
    assert_eq!(run("var a,b; [[a],{x:b}]=[[7],{x:8}]; a+','+b"), "7,8");
}

#[test]
fn object_literal_methods() {
    assert_eq!(run("({*g(){yield 1; yield 2}}).g().next().value"), "1");
    assert_eq!(run("[...({*g(){yield 1;yield 2}}).g()].join(',')"), "1,2");
    assert_eq!(
        run("({async m(){return 5}}).m() instanceof Promise"),
        "true"
    );
    assert_eq!(run("({async(){return 1}}).async()"), "1"); // method named async
    assert_eq!(run("({async:7}).async"), "7"); // property named async
}

#[test]
fn early_errors() {
    // These must be parse-phase SyntaxErrors (Err).
    for src in [
        "const x",
        "return 5",
        "break",
        "continue",
        "{break}",
        "while(0){} break",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // These must still work.
    assert_eq!(run("function f(){return 7} f()"), "7");
    assert_eq!(
        run("var s=0; for(var i=0;i<3;i++){ if(i==1) continue; s+=i; } s"),
        "2"
    );
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
    for src in [
        "Map()",
        "Set()",
        "WeakMap()",
        "WeakSet()",
        "Promise(()=>{})",
        "ArrayBuffer(8)",
        "SharedArrayBuffer(8)",
        "Int8Array(4)",
        "Float64Array(2)",
        "DataView(new ArrayBuffer(8))",
        "Proxy({},{})",
    ] {
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
    assert_eq!(
        run("class S extends Set{}; var s=new S([3,4]); s.has(3)+''+s.size"),
        "true2"
    );
    assert_eq!(
        run("class I extends Int8Array{}; var a=new I([5,6,7]); a[1]"),
        "6"
    );
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
    assert_eq!(
        run("Object.getOwnPropertyDescriptor({get x(){}},'x').get.name"),
        "get x"
    );
    assert_eq!(run("function named(){}; var x=named; x.name"), "named"); // keeps original
    assert_eq!(run("(function foo(){}).name"), "foo"); // named expr unchanged
}
#[test]
fn label_validation() {
    assert!(Engine::new().eval("break foo;", false).is_err());
    assert!(Engine::new().eval("x: x: 1", false).is_err());
    assert!(Engine::new()
        .eval("foo: for(;;){ continue bar; }", false)
        .is_err());
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
    for src in [
        "let x; let x",
        "{ let y; let y }",
        "let a; const a=1",
        "let b; var b",
        "{ let c; function c(){} }",
        "if(true) let z = 1",
        "while(false) const w = 1",
        "for(;;) let q",
        "label: let p = 1",
        "const d=1; let d",
        "function f(){ let e; let e }",
        "try{}catch(e){ let e }",
    ] {
        eprintln!(
            "RD {src:?} => {}",
            if crate::Engine::new().eval(src, false).is_err() {
                "SyntaxErr"
            } else {
                "ACCEPTED"
            }
        );
    }
    // These are fine.
    for src in [
        "let x; { let x }",
        "{let a}{let a}",
        "let m=1; m=2",
        "var n; var n",
    ] {
        eprintln!(
            "RDok {src:?} => {}",
            match crate::Engine::new().eval(src, false) {
                Ok(_) => "ok",
                Err(_) => "WRONGLY-REJECTED",
            }
        );
    }
}
#[test]
fn lexical_substatement() {
    for src in [
        "if(true) let z = 1",
        "while(false) const w = 1",
        "for(;;) let q",
        "label: let p = 1",
        "if(x) class C{}",
        "do let r=1; while(0)",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
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
    for src in [
        "let x; let x",
        "{ let y; let y }",
        "let a; const a=1",
        "let b; var b",
        "var bb; let bb",
        "let c; function c(){}",
        "const d=1; let d",
        "class E{}; let E",
        "switch(1){case 1: let s; default: let s}",
        "function z(){ let e; let e }",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // allowed (no false positives)
    for src in [
        "let x; { let x }",
        "{let a}{let a}",
        "var n; var n",
        "let m=1; m=2",
        "function f(){} function f(){}",
        "for(let i=0;i<2;i++){} for(let i=0;i<2;i++){}",
        "if(1){let p}else{let p}",
        "let q; function g(){ let q }",
        "switch(1){case 1:{let s} case 2:{let s}}",
        "try{}catch(x){let y}",
    ] {
        assert!(
            Engine::new().eval(src, false).is_ok(),
            "should accept: {src}"
        );
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
    assert_eq!(
        throws("(function(){ typeof r; let r; })()"),
        "ReferenceError"
    );
    assert_eq!(
        throws("(function(){ return a; let a; })()"),
        "ReferenceError"
    );
    // valid uses still work
    assert_eq!(run("let p=1; p"), "1");
    assert_eq!(run("const q=2; q+1"), "3");
    assert_eq!(run("function f(){ let m=5; return m; } f()"), "5");
    assert_eq!(run("var g=10; g"), "10");
    assert_eq!(run("let a=1; { let a=2; } a"), "1");
}
#[test]
fn property_order() {
    assert_eq!(
        run("Object.keys({2:'a',1:'b',x:'c',0:'d'}).join(',')"),
        "0,1,2,x"
    );
    assert_eq!(
        run("var o={b:1}; o.a=2; o[5]=3; o[1]=4; Object.keys(o).join(',')"),
        "1,5,b,a"
    );
    assert_eq!(
        run("var r=[]; for(var k in {x:1,2:2,1:3}) r.push(k); r.join(',')"),
        "1,2,x"
    );
    assert_eq!(
        run("JSON.stringify({2:'a',1:'b',x:'c'})"),
        "{\"1\":\"b\",\"2\":\"a\",\"x\":\"c\"}"
    );
    assert_eq!(
        run("Object.values({2:'a',10:'b',1:'c'}).join(',')"),
        "c,a,b"
    );
    assert_eq!(run("Object.keys({...{b:1,1:2,a:3}}).join(',')"), "1,b,a");
    assert_eq!(
        run("var o=Object.assign({},{c:1,1:2,a:3}); Object.keys(o).join(',')"),
        "1,c,a"
    );
}
#[test]
fn to_primitive_symbol() {
    assert_eq!(
        run("var o={[Symbol.toPrimitive](h){return h}}; o + ''"),
        "default"
    );
    assert_eq!(
        run("var o={[Symbol.toPrimitive](h){return h}}; String(o)"),
        "string"
    );
    assert_eq!(run("var o={[Symbol.toPrimitive](){return 5}}; o + 1"), "6");
    assert_eq!(
        run("var o={[Symbol.toPrimitive](){return 5n}}; o + 1n"),
        "6"
    );
    assert_eq!(
        run("var o={[Symbol.toPrimitive](){return 42}}; Number(o)"),
        "42"
    );
    assert_eq!(run("var o={valueOf(){return 9}}; o + 1"), "10");
    assert_eq!(
        throws("var o={[Symbol.toPrimitive](){return {}}}; o+1"),
        "TypeError"
    );
}
#[test]
fn date_toprimitive() {
    assert_eq!(run("typeof (new Date(0) + new Date(0))"), "string");
    assert_eq!(run("(new Date(0))[Symbol.toPrimitive]('number')"), "0");
    assert_eq!(
        run("typeof (new Date(0))[Symbol.toPrimitive]('string')"),
        "string"
    );
    assert_eq!(run("var d=new Date(0); (d - 0)"), "0"); // number hint via subtraction
}
#[test]
fn not_a_constructor() {
    for src in [
        "new (Math.max)()",
        "new (parseInt)()",
        "new (Object.keys)()",
        "new (Array.prototype.map)()",
        "new (Array.from)()",
        "new ([].forEach)()",
        "new (JSON.stringify)()",
        "new (String.prototype.slice)()",
    ] {
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
    assert_eq!(
        run("typeof Object.getOwnPropertyDescriptor(Array,Symbol.species).get"),
        "function"
    );
}
#[test]
fn array_from_fixes() {
    assert_eq!(run("Array.from([1,2,3]).join(',')"), "1,2,3");
    assert_eq!(run("Array.from('abc').join(',')"), "a,b,c");
    assert_eq!(run("Array.from([1,2],x=>x*2).join(',')"), "2,4");
    assert_eq!(
        run("Array.from([1],function(){return this.v},{v:9})[0]"),
        "9"
    );
    assert_eq!(throws("Array.from([], null)"), "TypeError");
    assert_eq!(throws("Array.from([], 5)"), "TypeError");
    assert_eq!(run("Array.from({length:2,0:'a',1:'b'}).join(',')"), "a,b");
    assert_eq!(run("Array.from.call(Object,[1,2]).length"), "2");
    assert_eq!(
        run("Array.from.call(Object,[1,2]).constructor===Object"),
        "true"
    );
}
#[test]
fn dataview_index_validation() {
    assert_eq!(
        throws("new DataView(new ArrayBuffer(8)).getInt32(-1)"),
        "RangeError"
    );
    assert_eq!(
        throws("new DataView(new ArrayBuffer(8)).getInt32(100)"),
        "RangeError"
    );
    assert_eq!(
        throws("new DataView(new ArrayBuffer(8)).getFloat64(1)"),
        "RangeError"
    );
    assert_eq!(
        throws("new DataView(new ArrayBuffer(8)).getBigInt64(-5)"),
        "RangeError"
    );
    assert_eq!(
        run("var d=new DataView(new ArrayBuffer(8)); d.setInt32(0,42); d.getInt32(0)"),
        "42"
    );
    assert_eq!(
        run("var a=[1,2]; Object.freeze(a); Object.isFrozen(a)"),
        "true"
    );
}
#[test]
fn frozen_array_throws() {
    assert_eq!(
        throws("'use strict'; var a=Object.freeze([1,2]); a.push(3)"),
        "TypeError"
    );
    assert_eq!(
        throws("'use strict'; var a=Object.freeze([1,2]); a.length=0"),
        "TypeError"
    );
    assert_eq!(
        throws("'use strict'; var a=Object.freeze([1,2]); a.pop()"),
        "TypeError"
    );
    assert_eq!(
        run("var a=Object.freeze([1,2]); try{a.push(3)}catch(e){} a.length"),
        "2"
    ); // sloppy: unchanged
    assert_eq!(run("var a=[1,2]; a.push(3); a.join(',')"), "1,2,3"); // normal still works
    assert_eq!(run("var a=[1,2,3]; a.length=1; a.join(',')"), "1");
}
#[test]
fn proto_wrapper_exotics() {
    assert_eq!(run("Number.prototype == 0"), "true");
    assert_eq!(run("Number.prototype.valueOf()"), "0");
    assert_eq!(run("String.prototype == ''"), "true");
    assert_eq!(run("String.prototype.length"), "0");
    assert_eq!(run("Boolean.prototype.valueOf()"), "false");
    assert_eq!(run("Number.prototype.toFixed(2)"), "0.00");
    assert_eq!(run("(5).toFixed(2)"), "5.00");
    assert_eq!(run("new Number(7) == 7"), "true");
}
#[test]
fn regex_validation() {
    for src in [
        "RegExp('a**')",
        "RegExp('?a')",
        "RegExp('*a')",
        "RegExp('[b-a]')",
        "RegExp('a{2,1}')",
        "RegExp('+')",
    ] {
        assert_eq!(throws(src), "SyntaxError", "should reject: {src}");
    }
    // valid patterns still compile
    assert_eq!(run("/a+b*/.test('aab')"), "true");
    assert_eq!(run("/a{2,3}/.test('aa')"), "true");
    assert_eq!(run("/[a-z]/.test('m')"), "true");
    assert_eq!(run("/a+?/.test('a')"), "true"); // lazy
    assert_eq!(run("/a{1,2}?/.source"), "a{1,2}?");
    assert_eq!(run("/[*+?]/.test('*')"), "true"); // quantifiers literal in class
    assert_eq!(run("/\\*/.test('*')"), "true"); // escaped
}
#[test]
fn poison_pill() {
    assert_eq!(throws("function f(){}; f.caller"), "TypeError");
    assert_eq!(throws("function f(){}; f.arguments"), "TypeError");
    assert_eq!(throws("(function(){}).caller"), "TypeError");
    assert_eq!(
        throws("'use strict'; function f(){ return f.caller; }; f()"),
        "TypeError"
    );
    // normal function members still work
    assert_eq!(run("function f(a,b){}; f.length"), "2");
    assert_eq!(run("function f(){}; f.name"), "f");
    assert_eq!(run("function f(){return 1}; f()"), "1");
}
#[test]
fn define_property_semantics() {
    // validation throws
    assert_eq!(throws("Object.defineProperty(5,'x',{})"), "TypeError");
    assert_eq!(
        throws("Object.defineProperty({},'x',{value:1,get(){}})"),
        "TypeError"
    );
    assert_eq!(throws("Object.defineProperty({},'x',{get:5})"), "TypeError");
    assert_eq!(throws("Object.defineProperty({},'x',5)"), "TypeError");
    // partial redefine keeps other fields
    assert_eq!(run("var o={}; Object.defineProperty(o,'x',{value:1,writable:true,enumerable:true,configurable:true}); Object.defineProperty(o,'x',{enumerable:false}); var d=Object.getOwnPropertyDescriptor(o,'x'); d.value+','+d.writable+','+d.enumerable"), "1,true,false");
    // non-configurable can't be redefined incompatibly
    assert_eq!(throws("var o={}; Object.defineProperty(o,'x',{value:1,configurable:false}); Object.defineProperty(o,'x',{value:2})"), "TypeError");
    assert_eq!(throws("var o={}; Object.defineProperty(o,'x',{value:1,configurable:false}); Object.defineProperty(o,'x',{configurable:true})"), "TypeError");
    // non-extensible
    assert_eq!(
        throws("var o=Object.preventExtensions({}); Object.defineProperty(o,'x',{value:1})"),
        "TypeError"
    );
    // Reflect returns false (no throw) on invariant failure
    assert_eq!(run("var o={}; Object.defineProperty(o,'x',{value:1,configurable:false}); Reflect.defineProperty(o,'x',{value:2})"), "false");
    // normal cases work
    assert_eq!(
        run("var o={}; Object.defineProperty(o,'x',{value:42}); o.x"),
        "42"
    );
    assert_eq!(
        run("var o={}; Object.defineProperty(o,'x',{get(){return 7}}); o.x"),
        "7"
    );
    assert_eq!(run("var o={}; Object.defineProperty(o,'x',{value:1,configurable:true}); Object.defineProperty(o,'x',{value:2}); o.x"), "2");
}
#[test]
fn coll_brand_checks() {
    for src in [
        "Set.prototype.clear.call({})",
        "Set.prototype.values.call({})",
        "Set.prototype.keys.call({})",
        "Map.prototype.entries.call({})",
        "Map.prototype.keys.call(5)",
    ] {
        assert_eq!(throws(src), "TypeError", "should reject: {src}");
    }
    assert_eq!(run("var s=new Set([1,2]); s.clear(); s.size"), "0");
    assert_eq!(run("[...new Map([[1,2]]).entries()][0].join(',')"), "1,2");
    assert_eq!(run("[...new Set([3,4]).values()].join(',')"), "3,4");
}
#[test]
fn string_lastindexof() {
    assert_eq!(run("'abcabc'.lastIndexOf('b')"), "4");
    assert_eq!(run("'abcabc'.lastIndexOf('b',3)"), "1");
    assert_eq!(run("'abcabc'.lastIndexOf('x')"), "-1");
    assert_eq!(run("'canal'.lastIndexOf('a')"), "3");
    assert_eq!(run("'hello'.lastIndexOf('')"), "5");
    assert_eq!(run("'ABC'.toLocaleLowerCase()"), "abc");
    assert_eq!(run("'abc'.toLocaleUpperCase()"), "ABC");
    assert_eq!(run("'abab'.lastIndexOf('ab')"), "2");
}
#[test]
fn arraylike_huge_length() {
    assert_eq!(
        run("Array.prototype.indexOf.call({0:0,length:Infinity},0)"),
        "0"
    );
    assert_eq!(
        run("Array.prototype.includes.call({0:5,length:Infinity},5)"),
        "true"
    );
    assert_eq!(
        run("Array.prototype.some.call({0:1,length:Infinity},x=>x===1)"),
        "true"
    );
    assert_eq!(
        run("Array.prototype.every.call({0:1,length:Infinity},x=>x!==1)"),
        "false"
    );
    assert_eq!(
        run("Array.prototype.find.call({0:7,length:Infinity},x=>x===7)"),
        "7"
    );
    assert_eq!(run("[1,2,3].indexOf(2)"), "1");
    assert_eq!(run("[1,2,3].includes(3)"), "true");
}
#[test]
fn typed_array_intrinsic() {
    assert_eq!(
        run("var TA=Object.getPrototypeOf(Int8Array); typeof TA.prototype.at"),
        "function"
    );
    assert_eq!(run("var TA=Object.getPrototypeOf(Int8Array); TA.prototype===Object.getPrototypeOf(Int8Array.prototype)"), "true");
    assert_eq!(
        run("Object.getPrototypeOf(Int8Array)===Object.getPrototypeOf(Float64Array)"),
        "true"
    );
    assert_eq!(
        run("var TA=Object.getPrototypeOf(Int8Array); TA.name"),
        "TypedArray"
    );
    assert_eq!(
        run("typeof Object.getPrototypeOf(Int8Array).from"),
        "function"
    );
    assert_eq!(
        throws("var TA=Object.getPrototypeOf(Int8Array); new TA()"),
        "TypeError"
    );
    assert_eq!(run("new Int8Array([1,2,3]).toLocaleString()"), "1,2,3");
    assert_eq!(run("new Int8Array([1,2,3]).at(-1)"), "3");
    assert_eq!(
        run("Object.getPrototypeOf(Int8Array)[Symbol.species]===Int8Array.constructor||true"),
        "true"
    );
}
#[test]
fn ta_returns_ta() {
    assert_eq!(
        run("new Int8Array([1,2,3]).map(x=>x*2).constructor.name"),
        "Int8Array"
    );
    assert_eq!(run("new Int8Array([1,2,3]).map(x=>x*2).join(',')"), "2,4,6");
    assert_eq!(
        run("new Uint8Array([1,2,3,4]).filter(x=>x%2===0).join(',')"),
        "2,4"
    );
    assert_eq!(
        run("new Int16Array([1,2,3]).slice(1).constructor.name"),
        "Int16Array"
    );
    assert_eq!(run("new Int8Array([1,2,3]).slice(1).join(',')"), "2,3");
    assert_eq!(
        run("new Float64Array([1.5,2.5]).map(x=>x).join(',')"),
        "1.5,2.5"
    );
    assert_eq!(
        run("new Int8Array([3,1,2]).toSorted().constructor.name"),
        "Int8Array"
    );
}
#[test]
fn iterator_close_destructure() {
    // Lazy: only pulls 2, closes the rest (would be infinite otherwise).
    assert_eq!(run("var n=0; var iter={[Symbol.iterator](){return {next(){return {value:n++,done:false}},return(){this.closed=true;return {}}}}}; var [a,b]=iter; a+','+b"), "0,1");
    assert_eq!(run("var closed=false; var iter={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){closed=true;return {}}}}}; var [a]=iter; closed"), "true");
    // rest consumes all (finite)
    assert_eq!(run("var [a,...r]=[1,2,3,4]; a+'/'+r.join(',')"), "1/2,3,4");
    assert_eq!(run("var [a,b,c]=[1,2]; a+','+b+','+c"), "1,2,undefined");
    assert_eq!(run("var [x=9]=[]; x"), "9");
    assert_eq!(run("for(var [k,v] of [[1,2],[3,4]]){} k+','+v"), "3,4");
    assert_eq!(run("var [,b]=[1,2]; b"), "2");
}
#[test]
fn forof_lazy_close() {
    // break closes the iterator (infinite otherwise)
    assert_eq!(run("var closed=false; var it={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){closed=true;return {}}}}}; for(var x of it){break;} closed"), "true");
    assert_eq!(run("var s=0; for(var x of [1,2,3]){s+=x} s"), "6");
    assert_eq!(
        run("var s=0; for(var x of [1,2,3,4,5]){ if(x>3)break; s+=x } s"),
        "6"
    );
    assert_eq!(run("var n=0; var it={[Symbol.iterator](){return {next(){return {value:n++,done:n>1000000000}}}}}; var c=0; for(var x of it){c++; if(c>=3)break;} c"), "3");
    assert_eq!(run("var r=''; for(var k of 'abc'){r+=k} r"), "abc");
}
#[test]
fn assign_destructure_close() {
    assert_eq!(run("var a,b; [a,b]=[1,2]; a+','+b"), "1,2");
    assert_eq!(run("var a,r; [a,...r]=[1,2,3]; a+'/'+r.join(',')"), "1/2,3");
    assert_eq!(run("var closed=false,a; var it={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){closed=true;return {}}}}}; [a]=it; closed"), "true");
    assert_eq!(run("var a,b; [a,,b]=[1,2,3]; a+','+b"), "1,3");
    assert_eq!(run("var x; [x=5]=[]; x"), "5");
}
#[test]
fn string_iterator() {
    assert_eq!(run("typeof String.prototype[Symbol.iterator]"), "function");
    assert_eq!(run("[...'abc'].join(',')"), "a,b,c");
    assert_eq!(
        run("var it='hi'[Symbol.iterator](); it.next().value+it.next().value"),
        "hi"
    );
    assert_eq!(run("var r=''; for(var c of 'xyz') r+=c; r"), "xyz");
}
#[test]
fn iterator_helpers() {
    assert_eq!(run("[...[1,2,3].values().map(x=>x*2)].join(',')"), "2,4,6");
    assert_eq!(
        run("[1,2,3,4].values().filter(x=>x%2===0).toArray().join(',')"),
        "2,4"
    );
    assert_eq!(
        run("[1,2,3,4,5].values().take(2).toArray().join(',')"),
        "1,2"
    );
    assert_eq!(
        run("[1,2,3,4,5].values().drop(2).toArray().join(',')"),
        "3,4,5"
    );
    assert_eq!(run("[1,2,3].values().reduce((a,b)=>a+b,0)"), "6");
    assert_eq!(run("[1,2,3].values().reduce((a,b)=>a+b)"), "6");
    assert_eq!(run("var s=0; [1,2,3].values().forEach(x=>s+=x); s"), "6");
    assert_eq!(run("[1,2,3].values().some(x=>x===2)"), "true");
    assert_eq!(run("[1,2,3].values().every(x=>x>0)"), "true");
    assert_eq!(run("[1,2,3].values().find(x=>x>1)"), "2");
    assert_eq!(run("typeof Iterator.prototype.map"), "function");
    assert_eq!(
        run("[1,2,3,4,5].values().filter(x=>x>1).take(2).toArray().join(',')"),
        "2,3"
    );
}
#[test]
fn temporal_round_string() {
    assert_eq!(
        run("Temporal.Duration.from({hours:2,minutes:30}).round('hour').toString()"),
        "PT3H"
    );
    assert_eq!(
        run("Temporal.Duration.from({hours:2,minutes:30}).total('minute')"),
        "150"
    );
    assert_eq!(
        run("new Temporal.PlainTime(3,30,0).round('hour').toString()"),
        "04:00:00"
    );
    assert_eq!(
        run("Temporal.Duration.from({minutes:90}).round('hours').toString()"),
        "PT2H"
    );
    // object form still works
    assert_eq!(
        run("new Temporal.PlainTime(3,30).round({smallestUnit:'hour'}).toString()"),
        "04:00:00"
    );
}
#[test]
fn reflect_construct_newtarget() {
    assert_eq!(run("function isC(f){try{Reflect.construct(function(){},[],f);return true}catch(e){return false}} isC(function(){})+','+isC(Math.max)+','+isC(Array)+','+isC(()=>{})"), "true,false,true,false");
    assert_eq!(run("Reflect.construct(Array,[1,2,3]).length"), "3");
    assert_eq!(throws("Reflect.construct(Math.max,[])"), "TypeError");
    assert_eq!(
        throws("Reflect.construct(function(){},[],Math.max)"),
        "TypeError"
    );
    assert_eq!(
        run("typeof Reflect.construct(function(){this.x=1},[])"),
        "object"
    );
    assert_eq!(
        run("class C{}; Reflect.construct(C,[]) instanceof C"),
        "true"
    );
}
#[test]
fn abstract_subclass() {
    assert_eq!(throws("new Iterator()"), "TypeError");
    assert_eq!(
        run("class MyIter extends Iterator { next(){return {done:true}} }; typeof new MyIter()"),
        "object"
    );
    assert_eq!(
        run("class MyIter extends Iterator {}; new MyIter() instanceof Iterator"),
        "true"
    );
    var_check();
}
fn var_check() {
    assert_eq!(run("var TA=Object.getPrototypeOf(Int8Array); class T extends Int8Array {}; new T(3).length"), "3");
}
#[test]
fn disposable_stack() {
    assert_eq!(run("typeof DisposableStack"), "function");
    assert_eq!(run("var log=''; var s=new DisposableStack(); s.use({[Symbol.dispose](){log+='a'}}); s.use({[Symbol.dispose](){log+='b'}}); s.dispose(); log"), "ba");
    assert_eq!(run("var s=new DisposableStack(); s.disposed"), "false");
    assert_eq!(
        run("var s=new DisposableStack(); s.dispose(); s.disposed"),
        "true"
    );
    assert_eq!(
        run("var log=''; var s=new DisposableStack(); s.defer(()=>log+='d'); s.dispose(); log"),
        "d"
    );
    assert_eq!(
        run("var log=''; var s=new DisposableStack(); s.adopt(5,v=>log+=v); s.dispose(); log"),
        "5"
    );
    assert_eq!(run("var s=new DisposableStack(); s.use({[Symbol.dispose](){}}); var s2=s.move(); s.disposed+','+s2.disposed"), "true,false");
    assert_eq!(run("typeof Symbol.dispose"), "symbol");
}
#[test]
fn regexp_symbol_methods() {
    assert_eq!(run("typeof RegExp.prototype[Symbol.replace]"), "function");
    assert_eq!(run("typeof RegExp.prototype[Symbol.match]"), "function");
    assert_eq!(run("/b/[Symbol.replace]('abc','X')"), "aXc");
    assert_eq!(run("/\\d/g[Symbol.match]('a1b2').join(',')"), "1,2");
    assert_eq!(run("/b/[Symbol.search]('abc')"), "1");
    assert_eq!(run("/,/[Symbol.split]('a,b,c').join('|')"), "a|b|c");
    assert_eq!(run("[.../\\d/g[Symbol.matchAll]('a1b2')].length"), "2");
    assert_eq!(
        throws("RegExp.prototype[Symbol.match].call({}, 'x')"),
        "TypeError"
    );
}
#[test]
fn regexp_proto_getters() {
    assert_eq!(run("/abc/gi.source"), "abc");
    assert_eq!(run("/abc/gi.flags"), "gi");
    assert_eq!(run("/abc/g.global"), "true");
    assert_eq!(run("/abc/.global"), "false");
    assert_eq!(run("RegExp.prototype.source"), "(?:)");
    assert_eq!(run("RegExp.prototype.flags"), "");
    assert_eq!(
        run("typeof Object.getOwnPropertyDescriptor(RegExp.prototype,'flags').get"),
        "function"
    );
    assert_eq!(
        run("typeof Object.getOwnPropertyDescriptor(RegExp.prototype,'source').get"),
        "function"
    );
    assert_eq!(run("/x/.hasOwnProperty('source')"), "false");
    assert_eq!(run("/x/g.lastIndex"), "0");
    assert_eq!(
        throws("Object.getOwnPropertyDescriptor(RegExp.prototype,'global').get.call({})"),
        "TypeError"
    );
    assert_eq!(run("/abc/d.hasIndices"), "true");
}
#[test]
fn date_format_methods() {
    assert_eq!(run("new Date(0).toDateString()"), "Thu Jan 01 1970");
    assert_eq!(
        run("new Date(0).toUTCString()"),
        "Thu, 01 Jan 1970 00:00:00 GMT"
    );
    assert_eq!(
        run("new Date(Date.UTC(2020,0,15,10,30,0)).toDateString()"),
        "Wed Jan 15 2020"
    );
    assert_eq!(run("new Date(0).toTimeString().slice(0,8)"), "00:00:00");
    assert_eq!(run("typeof new Date(0).toLocaleString()"), "string");
    assert_eq!(run("new Date(NaN).toDateString()"), "Invalid Date");
    assert_eq!(
        run("new Date(0).toGMTString()"),
        "Thu, 01 Jan 1970 00:00:00 GMT"
    );
}
#[test]
fn promise_combinators() {
    assert_eq!(run("typeof Promise.allSettled"), "function");
    assert_eq!(run("typeof Promise.any"), "function");
    assert_eq!(run("typeof AggregateError"), "function");
    assert_eq!(run("new AggregateError([1,2,3]).errors.length"), "3");
    assert_eq!(run("new AggregateError([],'msg').message"), "msg");
    assert_eq!(run("new AggregateError([1]) instanceof Error"), "true");
    assert_eq!(run("new AggregateError([1]).name"), "AggregateError");
}
#[test]
fn promise_combinators_async() {
    let mut e = Engine::new();
    e.eval("var r; Promise.allSettled([Promise.resolve(1),Promise.reject(2)]).then(v=>r=v.map(x=>x.status).join(','))", false).unwrap();
    assert_eq!(
        match e.eval("r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "fulfilled,rejected"
    );
    let mut e2 = Engine::new();
    e2.eval(
        "var r2; Promise.any([Promise.reject(1),Promise.resolve(9)]).then(v=>r2=v)",
        false,
    )
    .unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "9"
    );
}
#[test]
fn array_species() {
    assert_eq!(run("[1,2,3].map(x=>x*2).join(',')"), "2,4,6");
    assert_eq!(run("[1,2,3,4].filter(x=>x%2===0).join(',')"), "2,4");
    assert_eq!(run("[1,2,3,4,5].slice(1,3).join(',')"), "2,3");
    assert_eq!(
        run("class A extends Array {}; new A(1,2,3).map(x=>x).constructor.name"),
        "A"
    );
    assert_eq!(
        run("class A extends Array {}; new A(1,2,3).filter(()=>true) instanceof A"),
        "true"
    );
    assert_eq!(run("var a=[1,2]; a.constructor={[Symbol.species]:function(n){this.tag='X';return new Array(n)}}; var r=a.map(x=>x); typeof r"), "object");
    assert_eq!(throws("[1,2,3].map(5)"), "TypeError");
    assert_eq!(run("[1,2,3].map(x=>x).constructor.name"), "Array");
}
#[test]
fn arraylike_string_length() {
    assert_eq!(
        run("var r=0; Array.prototype.forEach.call({1:11,2:9,length:'2'},v=>{if(v>10)r=1}); r"),
        "1"
    );
    assert_eq!(
        run("Array.prototype.indexOf.call({0:'a',1:'b',length:'2'},'b')"),
        "1"
    );
    assert_eq!(
        run("Array.prototype.map.call({0:1,1:2,length:2},x=>x*2).join(',')"),
        "2,4"
    );
    assert_eq!(
        run("Array.prototype.join.call({0:'a',1:'b',length:{valueOf(){return 2}}},'-')"),
        "a-b"
    );
    assert_eq!(run("[1,2,3].forEach(()=>{}); 'ok'"), "ok");
    assert_eq!(
        run("Array.prototype.some.call({0:5,length:'1'},x=>x===5)"),
        "true"
    );
}
#[test]
fn sparse_array_holes() {
    assert_eq!(run("var c=0; [1,,3].forEach(()=>c++); c"), "2");
    assert_eq!(
        run("var a=[1,,3].map(x=>x*2); a.length+','+(1 in a)+','+a[0]+','+a[2]"),
        "3,false,2,6"
    );
    assert_eq!(run("[1,,3].filter(()=>true).length"), "2");
    assert_eq!(run("[1,,3].every(x=>x>0)"), "true");
    assert_eq!(run("[1,,3].some(x=>x===undefined)"), "false");
    assert_eq!(run("[1,2,3].map(x=>x*2).join(',')"), "2,4,6");
    assert_eq!(throws("[1,2,3].forEach(5)"), "TypeError");
}
#[test]
fn reduce_indexof_holes() {
    assert_eq!(run("[1,,3].reduce((a,b)=>a+b)"), "4");
    assert_eq!(run("[1,,3].reduce((a,b)=>a+b,0)"), "4");
    assert_eq!(run("[,,5].reduce((a,b)=>a+b)"), "5");
    assert_eq!(run("[1,2,3,2].indexOf(2)"), "1");
    assert_eq!(run("[1,2,3,2].indexOf(2,2)"), "3");
    assert_eq!(run("[1,2,3].indexOf(9)"), "-1");
    assert_eq!(throws("[].reduce((a,b)=>a+b)"), "TypeError");
    assert_eq!(throws("[1,2,3].reduce(5)"), "TypeError");
    assert_eq!(run("['a','b','c'].indexOf('c',-1)"), "2");
}
#[test]
fn accessor_arity() {
    for src in [
        "({get x(a){return 1}})",
        "({set x(){}})",
        "({set x(a,b){}})",
        "({set x(...r){}})",
        "class C{get x(a){}}",
        "class C{set x(){}}",
        "class C{set x(a,b){}}",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // valid
    assert_eq!(run("({get x(){return 5}}).x"), "5");
    assert_eq!(run("var v; var o={set x(n){v=n}}; o.x=7; v"), "7");
    assert_eq!(run("class C{get y(){return 3}}; new C().y"), "3");
    assert_eq!(run("({set x(v=1){}}); 'ok'"), "ok"); // default param allowed on setter
}
#[test]
fn template_octal_escape() {
    for src in [
        "`\\1`",
        "`\\01`",
        "`\\07`",
        "`a\\8b`",
        "`x\\9`",
        "`${1}\\1`",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    assert_eq!(run("`\\0`==='\\0'"), "true"); // lone NUL escape is fine
    assert_eq!(run("`a\\u0041b`"), "aAb");
    assert_eq!(run("`hi ${1+1}`"), "hi 2");
    assert_eq!(run("`\\t`.length"), "1");
}
#[test]
fn for_of_member_target() {
    assert_eq!(run("var o={}; for (o.p of [1,2,3]); o.p"), "3");
    assert_eq!(run("var o={}; for (o['k'] of [9]); o.k"), "9");
    assert_eq!(run("var a=[]; for ([a[0]] of [[5]]); a[0]"), "5");
    assert_eq!(run("var o={}; for (o.x in {a:1,b:2}); o.x"), "b");
    assert_eq!(run("var x; var s=''; for (x in {a:1,b:2}) s+=x; s"), "ab");
    assert_eq!(run("var o={}; [o.p]=[7]; o.p"), "7");
}
#[test]
fn for_head_no_in() {
    assert_eq!(run("var x; for (x in {a:1}); x"), "a");
    assert_eq!(run("for (var i=('x' in {x:1})?0:5; i<1; i++); i"), "1"); // `in` allowed in parens
    assert_eq!(
        run("var a={b:1}; for (var k=[('b' in a)]; false;); k[0]"),
        "true"
    ); // in inside []
    assert_eq!(run("var r=0; for (var i of [1,2,3]) r+=i; r"), "6");
    assert_eq!(run("var c=0; for (var k in {a:1,b:2,c:3}) c++; c"), "3");
    assert_eq!(run("'q' in {q:1}"), "true");
}
#[test]
fn tagged_templates() {
    assert_eq!(run("function t(s){return s[0]} t`hi`"), "hi");
    assert_eq!(run("function t(s,a){return s[0]+a+s[1]} t`x${5}y`"), "x5y");
    assert_eq!(run("function t(s){return s.raw[0]} t`a\\nb`"), "a\\nb");
    assert_eq!(run("function t(s){return s.length} t`a${1}b${2}c`"), "3");
    assert_eq!(run("function t(s){return s[0]} t`a\\nb`"), "a\nb");
    assert_eq!(
        run("function t(s){return Object.isFrozen(s)&&Object.isFrozen(s.raw)} t`x`"),
        "true"
    );
    assert_eq!(run("var o={m(s){return s[0]}}; o.m`hi`"), "hi");
    assert_eq!(run("typeof String.raw"), "function");
    assert_eq!(run("String.raw`a\\nb`"), "a\\nb");
    assert_eq!(run("String.raw`${1}+${2}`"), "1+2");
}
#[test]
fn bigint_prop_names() {
    assert_eq!(run("({1n:5})[1]"), "5");
    assert_eq!(run("({1n:5})['1']"), "5");
    assert_eq!(run("({100n:'x'})[100]"), "x");
    assert_eq!(run("var o={2n:'a',3n:'b'}; o[2]+o[3]"), "ab");
    assert_eq!(run("class C{1n=9}; new C()[1]"), "9");
}
#[test]
fn optional_chaining() {
    assert_eq!(run("var f=null; f?.()"), "undefined");
    assert_eq!(run("var a=null; a?.b.c.d"), "undefined"); // whole chain short-circuits
    assert_eq!(run("var a={b:null}; a?.b?.c"), "undefined");
    assert_eq!(run("var a={b:{c:5}}; a?.b?.c"), "5");
    assert_eq!(run("var a=null; a?.b['x'].y"), "undefined");
    assert_eq!(run("var o={m(){return 7}}; o?.m()"), "7");
    assert_eq!(run("var o=null; o?.m()"), "undefined");
    assert_eq!(run("var o={a:{b(){return 3}}}; o?.a.b()"), "3");
    assert_eq!(run("var o={f:null}; o.f?.()"), "undefined");
    assert_eq!(run("var x={y:{z:1}}; (x?.y).z"), "1");
    assert_eq!(throws("var a=null; (a?.b).c"), "TypeError"); // parens end the chain → .c on undefined throws
    assert_eq!(run("var a={b:1}; a?.b"), "1");
}
#[test]
fn private_in() {
    assert_eq!(
        run("class C{#x=1; static has(o){return #x in o}} C.has(new C())"),
        "true"
    );
    assert_eq!(
        run("class C{#x=1; static has(o){return #x in o}} C.has({})"),
        "false"
    );
    assert_eq!(
        run("class C{#m(){} static has(o){return #m in o}} C.has(new C())"),
        "true"
    );
    assert_eq!(
        run("class C{#x; static check(o){return #x in o}} C.check(new C())+','+C.check([])"),
        "true,false"
    );
    assert_eq!(
        throws("class C{#x=1; static has(o){return #x in o}} C.has(5)"),
        "TypeError"
    );
    assert_eq!(run("class C{#x=1; t(){return this.#x}} new C().t()"), "1");
}
#[test]
fn split_limit_and_radix() {
    assert_eq!(run("'a,b,c'.split(',',2).join('|')"), "a|b");
    assert_eq!(run("'a,b,c'.split(',',0).length"), "0");
    assert_eq!(run("'a,b,c,d'.split(',',2).join('|')"), "a|b");
    assert_eq!(run("'abc'.split('',2).join('|')"), "a|b");
    assert_eq!(run("'abc'.split(/(?:)/).length"), "3");
    assert_eq!(run("'a,b,c'.split(',').length"), "3");
    assert_eq!(run("(255).toString(16)"), "ff");
    assert_eq!(run("(3.5).toString(2)"), "11.1");
    assert_eq!(run("(0.5).toString(2)"), "0.1");
    assert_eq!(run("(NaN).toString()"), "NaN");
    assert_eq!(throws("(10).toString(37)"), "RangeError");
    assert_eq!(throws("(10).toString(1)"), "RangeError");
    assert_eq!(run("(255).toString(2)"), "11111111");
}
#[test]
fn proxy_traps() {
    assert_eq!(run("var log=''; var p=new Proxy({},{getPrototypeOf(t){log+='gp';return Array.prototype}}); Object.getPrototypeOf(p)===Array.prototype && log==='gp'"), "true");
    assert_eq!(run("var p=new Proxy({},{ownKeys(){return ['a','b']}}); Object.getOwnPropertyNames(p).join(',')"), "a,b");
    assert_eq!(
        run("var p=new Proxy({},{ownKeys(){return ['a','b']}}); Reflect.ownKeys(p).join(',')"),
        "a,b"
    );
    assert_eq!(
        run("var p=new Proxy({},{getPrototypeOf(){return null}}); Object.getPrototypeOf(p)"),
        "null"
    );
    assert_eq!(
        throws("var p=new Proxy({},{getPrototypeOf(){return 5}}); Object.getPrototypeOf(p)"),
        "TypeError"
    );
    assert_eq!(
        throws("var p=new Proxy({},{ownKeys(){return [1,2]}}); Object.getOwnPropertyNames(p)"),
        "TypeError"
    );
    assert_eq!(
        run("var p=new Proxy({a:1,b:2},{}); Object.getOwnPropertyNames(p).join(',')"),
        "a,b"
    ); // no trap forwards
    assert_eq!(
        run("var p=new Proxy([1,2],{}); Object.getPrototypeOf(p)===Array.prototype"),
        "true"
    );
    assert_eq!(run("Object.getPrototypeOf('x')===String.prototype"), "true");
}
#[test]
fn proxy_gopd_trap() {
    assert_eq!(run("var p=new Proxy({},{getOwnPropertyDescriptor(t,k){return {value:42,configurable:true}}}); Object.getOwnPropertyDescriptor(p,'x').value"), "42");
    assert_eq!(run("var p=new Proxy({},{getOwnPropertyDescriptor(){return undefined}}); Object.getOwnPropertyDescriptor(p,'x')"), "undefined");
    assert_eq!(
        run("var p=new Proxy({a:5},{}); Object.getOwnPropertyDescriptor(p,'a').value"),
        "5"
    );
    assert_eq!(run("var log=''; var p=new Proxy({},{getOwnPropertyDescriptor(t,k){log+=k;return {value:1,configurable:true}}}); Object.getOwnPropertyDescriptor(p,'foo'); log"), "foo");
    assert_eq!(run("var p=new Proxy({},{getOwnPropertyDescriptor(){return {value:9,configurable:true}}}); Object.getOwnPropertyDescriptor(p,'x').writable"), "false");
}
#[test]
fn proxy_defineprop_trap() {
    assert_eq!(run("var log=''; var p=new Proxy({},{defineProperty(t,k,d){log+=k+':'+d.value;return true}}); Object.defineProperty(p,'x',{value:7}); log"), "x:7");
    assert_eq!(throws("var p=new Proxy({},{defineProperty(){return false}}); Object.defineProperty(p,'x',{value:1})"), "TypeError");
    assert_eq!(run("var p=new Proxy({},{defineProperty(){return true}}); Reflect.defineProperty(p,'x',{value:1})"), "true");
    assert_eq!(run("var p=new Proxy({},{defineProperty(){return false}}); Reflect.defineProperty(p,'x',{value:1})"), "false");
    assert_eq!(run("var t={}; var p=new Proxy(t,{}); Object.defineProperty(p,'a',{value:5,configurable:true}); t.a"), "5");
}
#[test]
fn proxy_delete_trap() {
    assert_eq!(run("var log=''; var p=new Proxy({},{deleteProperty(t,k){log+=k;return true}}); delete p.x; log"), "x");
    assert_eq!(
        run("var p=new Proxy({},{deleteProperty(){return false}}); delete p.x"),
        "false"
    );
    assert_eq!(
        run("var t={a:1}; var p=new Proxy(t,{}); delete p.a; 'a' in t"),
        "false"
    );
    assert_eq!(
        run("var p=new Proxy({},{deleteProperty(){return true}}); delete p['k']"),
        "true"
    );
}
#[test]
fn proxy_misc_traps() {
    assert_eq!(run("var log=''; var p=new Proxy({},{setPrototypeOf(t,pr){log+='sp';return true}}); Object.setPrototypeOf(p,null); log"), "sp");
    assert_eq!(
        throws("var p=new Proxy({},{setPrototypeOf(){return false}}); Object.setPrototypeOf(p,{})"),
        "TypeError"
    );
    assert_eq!(run("var t={};Object.preventExtensions(t);var p=new Proxy(t,{isExtensible(){return false}}); Object.isExtensible(p)"), "false");
    assert_eq!(run("var log=''; var p=new Proxy({},{preventExtensions(t){log+='pe';Object.preventExtensions(t);return true}}); Object.preventExtensions(p); log"), "pe");
    assert_eq!(
        throws(
            "var p=new Proxy({},{preventExtensions(){return false}}); Object.preventExtensions(p)"
        ),
        "TypeError"
    );
    assert_eq!(throws("Object.setPrototypeOf({},5)"), "TypeError");
    assert_eq!(run("var t={}; var p=new Proxy(t,{}); Object.setPrototypeOf(p,Array.prototype); Object.getPrototypeOf(t)===Array.prototype"), "true");
}
#[test]
fn proxy_keys() {
    assert_eq!(
        run("var p=new Proxy({a:1,b:2},{}); Object.keys(p).join(',')"),
        "a,b"
    );
    assert_eq!(run("var p=new Proxy({},{ownKeys(){return ['x','y']},getOwnPropertyDescriptor(t,k){return {value:1,enumerable:true,configurable:true}}}); Object.keys(p).join(',')"), "x,y");
    assert_eq!(run("var p=new Proxy({},{ownKeys(){return ['x','y']},getOwnPropertyDescriptor(t,k){return {value:1,enumerable:k==='x',configurable:true}}}); Object.keys(p).join(',')"), "x");
}
#[test]
fn set_methods() {
    assert_eq!(
        run("[...new Set([1,2,3]).union(new Set([3,4]))].join(',')"),
        "1,2,3,4"
    );
    assert_eq!(
        run("[...new Set([1,2,3]).intersection(new Set([2,3,4]))].join(',')"),
        "2,3"
    );
    assert_eq!(
        run("[...new Set([1,2,3]).difference(new Set([2,3]))].join(',')"),
        "1"
    );
    assert_eq!(
        run("[...new Set([1,2,3]).symmetricDifference(new Set([3,4]))].join(',')"),
        "1,2,4"
    );
    assert_eq!(run("new Set([1,2]).isSubsetOf(new Set([1,2,3]))"), "true");
    assert_eq!(
        run("new Set([1,2,4]).isSubsetOf(new Set([1,2,3]))"),
        "false"
    );
    assert_eq!(run("new Set([1,2,3]).isSupersetOf(new Set([1,2]))"), "true");
    assert_eq!(run("new Set([1,2]).isDisjointFrom(new Set([3,4]))"), "true");
    assert_eq!(
        run("new Set([1,2]).isDisjointFrom(new Set([2,3]))"),
        "false"
    );
    assert_eq!(
        run("new Set([1,2,3]).union(new Set([3,4])) instanceof Set"),
        "true"
    );
    assert_eq!(throws("new Set([1]).union(5)"), "TypeError");
}
#[test]
fn iterator_flatmap() {
    assert_eq!(
        run("[1,2,3].values().flatMap(x=>[x,x*10]).toArray().join(',')"),
        "1,10,2,20,3,30"
    );
    assert_eq!(
        run("[1,2].values().flatMap(x=>[x]).toArray().join(',')"),
        "1,2"
    );
    assert_eq!(
        run("['a','b'].values().flatMap(s=>[s]).toArray().join(',')"),
        "a,b"
    );
    assert_eq!(run("[1,2,3].values().flatMap(x=>[]).toArray().length"), "0");
    assert_eq!(run("typeof Iterator.prototype.flatMap"), "function");
    assert_eq!(
        run("var c=0;[1,2].values().flatMap((x,i)=>{c=i;return[x]}).toArray();c"),
        "1"
    );
}
#[test]
fn map_getorinsert() {
    assert_eq!(
        run("var m=new Map(); m.getOrInsert('a',1); m.get('a')"),
        "1"
    );
    assert_eq!(run("var m=new Map([['a',5]]); m.getOrInsert('a',9)"), "5");
    assert_eq!(
        run("var m=new Map(); m.getOrInsertComputed('k',x=>x+'!'); m.get('k')"),
        "k!"
    );
    assert_eq!(
        run("var m=new Map([['k',2]]); m.getOrInsertComputed('k',()=>99)"),
        "2"
    );
    assert_eq!(
        run("var m=new Map(); m.getOrInsert('a',1); m.getOrInsert('a',2); m.get('a')"),
        "1"
    );
    assert_eq!(run("var m=new Map(); m.getOrInsert('x',7); m.size"), "1");
}
#[test]
fn promise_try_regexp_escape() {
    assert_eq!(run("typeof Promise.try"), "function");
    let mut e = Engine::new();
    e.eval("var r; Promise.try((a,b)=>a+b,2,3).then(v=>r=v)", false)
        .unwrap();
    assert_eq!(
        match e.eval("r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "5"
    );
    let mut e2 = Engine::new();
    e2.eval(
        "var r2; Promise.try(()=>{throw new Error('x')}).catch(e=>r2=e.message)",
        false,
    )
    .unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "x"
    );
    assert_eq!(run("typeof RegExp.escape"), "function");
    assert_eq!(run("RegExp.escape('a.b')"), "\\x61\\.b");
    assert_eq!(run("RegExp.escape('.*+')"), "\\.\\*\\+");
    assert_eq!(run("new RegExp(RegExp.escape('a.b')).test('a.b')"), "true");
    assert_eq!(run("new RegExp(RegExp.escape('a.b')).test('axb')"), "false");
    assert_eq!(throws("RegExp.escape(5)"), "TypeError");
}
#[test]
fn uint8_base64_hex() {
    assert_eq!(run("new Uint8Array([72,105]).toHex()"), "4869");
    assert_eq!(run("new Uint8Array([255,0,16]).toHex()"), "ff0010");
    assert_eq!(run("Uint8Array.fromHex('4869').join(',')"), "72,105");
    assert_eq!(run("new Uint8Array([72,105]).toBase64()"), "SGk=");
    assert_eq!(run("Uint8Array.fromBase64('SGk=').join(',')"), "72,105");
    assert_eq!(run("new Uint8Array([255,255]).toBase64()"), "//8=");
    assert_eq!(
        run("new Uint8Array([255,255]).toBase64({alphabet:'base64url'})"),
        "__8="
    );
    assert_eq!(
        run("new Uint8Array([72,105]).toBase64({omitPadding:true})"),
        "SGk"
    );
    assert_eq!(run("Uint8Array.fromBase64('SGVsbG8=').length"), "5");
    assert_eq!(run("typeof Uint8Array.prototype.toBase64"), "function");
    assert_eq!(
        run("var r=Uint8Array.fromHex('48656c6c6f'); String.fromCharCode(...r)"),
        "Hello"
    );
    assert_eq!(run("typeof Symbol.metadata"), "symbol");
}
#[test]
fn uint8_setfrom() {
    assert_eq!(run("var a=new Uint8Array(4); var r=a.setFromHex('41424344'); a.join(',')+'/'+r.written+','+r.read"), "65,66,67,68/4,8");
    assert_eq!(
        run("var a=new Uint8Array(2); a.setFromHex('414243'); a.join(',')"),
        "65,66"
    );
    assert_eq!(
        run("var a=new Uint8Array(3); a.setFromBase64('SGk='); a.join(',')"),
        "72,105,0"
    );
}
#[test]
fn float16_array() {
    // f16 round-trip correctness against known values.
    assert_eq!(run("Math.f16round(1)"), "1");
    assert_eq!(run("Math.f16round(0.5)"), "0.5");
    assert_eq!(run("Math.f16round(2)"), "2");
    assert_eq!(run("Math.f16round(1.337)"), "1.3369140625");
    assert_eq!(run("Math.f16round(1e10)"), "Infinity");
    assert_eq!(run("Math.f16round(-0)"), "0"); // -0 prints as 0
    assert_eq!(run("Object.is(Math.f16round(-0),-0)"), "true");
    assert_eq!(run("typeof Float16Array"), "function");
    assert_eq!(run("Float16Array.BYTES_PER_ELEMENT"), "2");
    assert_eq!(run("new Float16Array([1,2,3]).length"), "3");
    assert_eq!(run("new Float16Array([1.5,2.5])[1]"), "2.5");
    assert_eq!(
        run("var a=new Float16Array(2); a[0]=1.337; a[0]"),
        "1.3369140625"
    );
    assert_eq!(run("new Float16Array([0.1])[0]"), "0.0999755859375");
    assert_eq!(run("new Float16Array([65504])[0]"), "65504"); // max f16
    assert_eq!(run("new Float16Array([NaN])[0]"), "NaN");
}
#[test]
fn dataview_float16() {
    assert_eq!(
        run("var d=new DataView(new ArrayBuffer(2)); d.setFloat16(0,1.5); d.getFloat16(0)"),
        "1.5"
    );
    assert_eq!(run("typeof DataView.prototype.getFloat16"), "function");
    assert_eq!(
        run("var d=new DataView(new ArrayBuffer(2)); d.setFloat16(0,1.337); d.getFloat16(0)"),
        "1.3369140625"
    );
}
#[test]
fn async_disposable_stack() {
    assert_eq!(run("typeof AsyncDisposableStack"), "function");
    assert_eq!(run("typeof Symbol.asyncDispose"), "symbol");
    assert_eq!(run("var s=new AsyncDisposableStack(); s.disposed"), "false");
    assert_eq!(
        run("typeof new AsyncDisposableStack()[Symbol.asyncDispose]"),
        "function"
    );
    let mut e = Engine::new();
    e.eval("var log=''; var s=new AsyncDisposableStack(); s.defer(()=>{log+='a'}); s.defer(()=>{log+='b'}); s.disposeAsync().then(()=>log+='!')", false).unwrap();
    assert_eq!(
        match e.eval("log", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "ba!"
    );
    assert_eq!(run("var s=new AsyncDisposableStack(); s.use({[Symbol.asyncDispose](){}}); var s2=s.move(); s.disposed+','+s2.disposed"), "true,false");
}
#[test]
fn detached_typedarray() {
    assert_eq!(
        run("var a=new Int8Array(4); $262.detachArrayBuffer(a.buffer); a.length"),
        "0"
    );
    assert_eq!(
        run("var a=new Int8Array(4); $262.detachArrayBuffer(a.buffer); a.byteLength"),
        "0"
    );
    assert_eq!(
        run("var a=new Int8Array(4); $262.detachArrayBuffer(a.buffer); a[0]"),
        "undefined"
    );
    assert_eq!(
        throws("var a=new Int8Array([1,2,3]); $262.detachArrayBuffer(a.buffer); a.fill(0)"),
        "TypeError"
    );
    assert_eq!(
        throws("var a=new Int8Array([3,1,2]); $262.detachArrayBuffer(a.buffer); a.sort()"),
        "TypeError"
    );
    assert_eq!(
        throws("var a=new Int8Array(4); $262.detachArrayBuffer(a.buffer); a.join()"),
        "TypeError"
    );
    assert_eq!(run("var a=new Int8Array(4); a.length"), "4");
    assert_eq!(run("var a=new Int32Array(4); a.byteLength"), "16");
    assert_eq!(
        run("var a=new Int8Array([1,2,3]); a.fill(9); a.join(',')"),
        "9,9,9"
    );
}
#[test]
fn ta_index_properties() {
    assert_eq!(run("var a=new Int8Array(3); Object.defineProperty(a,'0',{value:7,writable:true,enumerable:true,configurable:true}); a[0]"), "7");
    assert_eq!(run("var a=new Int8Array(3); var d=Object.getOwnPropertyDescriptor(a,'0'); d.value+','+d.writable+','+d.enumerable+','+d.configurable"), "0,true,true,true");
    assert_eq!(run("new Int8Array(3).hasOwnProperty('0')"), "true");
    assert_eq!(run("new Int8Array([1,2,3]).hasOwnProperty('5')"), "false");
    assert_eq!(
        run("Object.getOwnPropertyNames(new Int8Array(3)).join(',')"),
        "0,1,2"
    );
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(new Int8Array(3),'5')"),
        "undefined"
    );
    assert_eq!(
        throws("Object.defineProperty(new Int8Array(3),'5',{value:1})"),
        "TypeError"
    );
    assert_eq!(
        run("var a=new Int8Array([1,2,3]); a.length+','+a.byteLength"),
        "3,3"
    );
}
#[test]
fn annexb_block_func_conflict() {
    // Conflicting intervening `let` → no function-scope var is synthesized.
    assert_eq!(
        throws("{ let f = 1; { function f(){} } } f"),
        "ReferenceError"
    );
    assert_eq!(
        run("{ let f = 1; { function f(){} } } typeof f"),
        "undefined"
    );
    // No conflict → the block function IS hoisted to function scope.
    assert_eq!(run("{ function g(){return 5} } typeof g"), "function");
    assert_eq!(run("{ { function h(){return 1} } } h()"), "1");
    // Conflict with const too.
    assert_eq!(
        throws("{ const c = 1; { function c(){} } } c()"),
        "ReferenceError"
    );
}
#[test]
fn modules_basic() {
    use std::collections::HashMap;
    let mut files: HashMap<String, String> = HashMap::new();
    files.insert(
        "/mod.js".into(),
        "export const x = 5; export function add(a,b){return a+b} export default 42;".into(),
    );
    files.insert(
        "/main.js".into(),
        "import def, {x, add} from '/mod.js'; globalThis.__r = def + x + add(1,2);".into(),
    );
    files.insert("/ns.js".into(), "import * as ns from '/mod.js'; globalThis.__r2 = ns.x + ns.add(2,3) + (typeof ns.default);".into());
    let f1 = files.clone();
    let mut e = Engine::new();
    e.eval_module(&f1["/main.js"].clone(), "/main.js", move |spec, _ref| {
        f1.get(spec).map(|s| (spec.to_string(), s.clone()))
    })
    .unwrap();
    assert_eq!(
        match e.eval("globalThis.__r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "50"
    ); // 42+5+3
    let f2 = files.clone();
    let mut e2 = Engine::new();
    e2.eval_module(&f2["/ns.js"].clone(), "/ns.js", move |spec, _ref| {
        f2.get(spec).map(|s| (spec.to_string(), s.clone()))
    })
    .unwrap();
    assert_eq!(
        match e2.eval("globalThis.__r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "10number"
    ); // 5+5+number
}
#[test]
fn modules_live_bindings() {
    use std::collections::HashMap;
    let mut files: HashMap<String, String> = HashMap::new();
    files.insert(
        "/counter.js".into(),
        "export let count = 0; export function inc(){ count++; }".into(),
    );
    files.insert("/main.js".into(), "import {count, inc} from '/counter.js'; import * as ns from '/counter.js'; inc(); inc(); globalThis.__r = count + ':' + ns.count;".into());
    let f = files.clone();
    let mut e = Engine::new();
    e.eval_module(&f["/main.js"].clone(), "/main.js", move |spec, _r| {
        f.get(spec).map(|s| (spec.to_string(), s.clone()))
    })
    .unwrap();
    assert_eq!(
        match e.eval("globalThis.__r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "2:2"
    );
}
#[test]
fn global_object_sync() {
    assert_eq!(
        run("function f(){return 5}; globalThis.hasOwnProperty('f')+','+globalThis.f()"),
        "true,5"
    );
    assert_eq!(
        run("var x=10; globalThis.hasOwnProperty('x')+','+globalThis.x"),
        "true,10"
    );
    assert_eq!(run("var x=1; x=2; globalThis.x"), "2");
    assert_eq!(run("globalThis.y=7; y"), "7");
    assert_eq!(run("let z=1; globalThis.hasOwnProperty('z')"), "false");
    assert_eq!(run("var a; globalThis.a=3; a"), "3");
    assert_eq!(run("typeof globalThis.Object"), "function"); // builtins still there
    assert_eq!(run("var undefined; typeof undefined"), "undefined"); // non-writable global kept
}
#[test]
fn array_from_async() {
    assert_eq!(run("typeof Array.fromAsync"), "function");
    let mut e = Engine::new();
    e.eval(
        "var r; Array.fromAsync([1,2,3]).then(a=>r=a.join(','))",
        false,
    )
    .unwrap();
    assert_eq!(
        match e.eval("r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "1,2,3"
    );
    let mut e2 = Engine::new();
    e2.eval(
        "var r2; Array.fromAsync([Promise.resolve(5),6]).then(a=>r2=a.join(','))",
        false,
    )
    .unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "5,6"
    );
    let mut e3 = Engine::new();
    e3.eval(
        "var r3; Array.fromAsync([1,2,3], x=>x*2).then(a=>r3=a.join(','))",
        false,
    )
    .unwrap();
    assert_eq!(
        match e3.eval("r3", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "2,4,6"
    );
    let mut e4 = Engine::new();
    e4.eval("async function* g(){yield 1; yield 2;} var r4; Array.fromAsync(g()).then(a=>r4=a.join(','))", false).unwrap();
    assert_eq!(
        match e4.eval("r4", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "1,2"
    );
}
#[test]
fn promise_keyed() {
    assert_eq!(run("typeof Promise.allKeyed"), "function");
    let mut e = Engine::new();
    e.eval("var r; Promise.allKeyed({a:Promise.resolve(1),b:2}).then(o=>r=o.a+','+o.b+','+(Object.getPrototypeOf(o)===null))", false).unwrap();
    assert_eq!(
        match e.eval("r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "1,2,true"
    );
    let mut e2 = Engine::new();
    e2.eval("var r2; Promise.allSettledKeyed({a:Promise.resolve(1),b:Promise.reject(9)}).then(o=>r2=o.a.status+','+o.a.value+','+o.b.status+','+o.b.reason)", false).unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "fulfilled,1,rejected,9"
    );
    let mut e3 = Engine::new();
    e3.eval(
        "var r3; Promise.allKeyed(5).catch(e=>r3=e.constructor.name)",
        false,
    )
    .unwrap();
    assert_eq!(
        match e3.eval("r3", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "TypeError"
    );
}
#[test]
fn async_generators() {
    assert_eq!(
        run("async function* g(){yield 1} typeof g().next().then"),
        "function"
    );
    assert_eq!(
        run("async function* g(){yield 1} typeof g()[Symbol.asyncIterator]"),
        "function"
    );
    assert_eq!(
        run("async function* g(){yield 1} typeof g().return"),
        "function"
    );
    assert_eq!(run("var s=''; async function* g(){yield 'a';yield 'b'} var it=g(); it.next().then(r=>s=r.value); 'ok'"), "ok");
    assert_eq!(
        run("function* g(){yield 1} var it=g(); it.next().value+','+it.next().done"),
        "1,true"
    );
    assert_eq!(run("function* g(){yield 1;yield 2} var it=g(); it.next(); it.return(9).value+','+it.next().done"), "9,true");
}
#[test]
fn for_await_of() {
    let mut e = Engine::new();
    e.eval("async function* g(){yield 1;yield 2;yield 3} (async()=>{ var s=0; for await (const x of g()) s+=x; globalThis.R=s; })()", false).unwrap();
    assert_eq!(
        match e.eval("globalThis.R", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "6"
    );
    let mut e2 = Engine::new();
    e2.eval("(async()=>{ var s=''; for await (const x of [Promise.resolve('a'),'b']) s+=x; globalThis.R2=s; })()", false).unwrap();
    assert_eq!(
        match e2.eval("globalThis.R2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "ab"
    );
}
#[test]
fn promise_combinator_reject_noniterable() {
    for m in ["all", "race", "allSettled", "any"] {
        let mut e = Engine::new();
        e.eval(
            &format!("var r; Promise.{m}(false).then(()=>r='F', e=>r=e.constructor.name)"),
            false,
        )
        .unwrap();
        assert_eq!(
            match e.eval("r", false).unwrap() {
                Completion::Value(v) => v,
                _ => String::new(),
            },
            "TypeError",
            "Promise.{} should reject",
            m
        );
    }
    let mut e2 = Engine::new();
    e2.eval(
        "var r2; Promise.all([1,2,3]).then(a=>r2=a.join(','))",
        false,
    )
    .unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "1,2,3"
    );
}
#[test]
fn promise_all_user_then() {
    let mut e = Engine::new();
    e.eval("var p=new Promise(function(){}); var err=new TypeError('x'); Object.defineProperty(p,'then',{value:function(){throw err}}); var r; Promise.all([p]).then(()=>r='F', reason=>r=(reason===err)?'OK':'wrong')", false).unwrap();
    assert_eq!(
        match e.eval("r", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "OK"
    );
    let mut e2 = Engine::new();
    e2.eval(
        "var r2; Promise.all([Promise.resolve(1),Promise.resolve(2)]).then(a=>r2=a.join(','))",
        false,
    )
    .unwrap();
    assert_eq!(
        match e2.eval("r2", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "1,2"
    );
    let mut e3 = Engine::new();
    e3.eval(
        "var r3; Promise.race([Promise.resolve('a'),Promise.resolve('b')]).then(v=>r3=v)",
        false,
    )
    .unwrap();
    assert_eq!(
        match e3.eval("r3", false).unwrap() {
            Completion::Value(v) => v,
            _ => String::new(),
        },
        "a"
    );
}
#[test]
fn async_label_dup_param() {
    assert!(Engine::new()
        .eval("async function f(){ await: 1; }", false)
        .is_err());
    assert!(Engine::new()
        .eval("function* g(){ yield: 1; }", false)
        .is_err());
    assert!(Engine::new().eval("var f = (a,a)=>1", false).is_err());
    assert!(Engine::new().eval("var f = (a,b,a)=>1", false).is_err());
    assert_eq!(run("var f = (a,b)=>a+b; f(1,2)"), "3");
    assert_eq!(run("function f(){ foo: 1; return 2 } f()"), "2"); // normal label ok
    assert_eq!(
        run("async function f(){ x: 1; return 5 } typeof f"),
        "function"
    ); // non-await label ok in async
}
#[test]
fn update_target_errors() {
    assert!(Engine::new().eval("0++", false).is_err());
    assert!(Engine::new().eval("++0", false).is_err());
    assert!(Engine::new().eval("(a+b)++", false).is_err());
    assert!(Engine::new().eval("'x'--", false).is_err());
    assert_eq!(run("var a=5; a++; a"), "6");
    assert_eq!(run("var o={x:1}; o.x++; o.x"), "2");
    assert_eq!(run("var a=[1]; a[0]++; a[0]"), "2");
}
#[test]
fn new_target_context() {
    assert!(Engine::new().eval("new.target", false).is_err());
    assert!(Engine::new().eval("new.foo", false).is_err());
    assert_eq!(
        run("function f(){ return typeof new.target } f()"),
        "undefined"
    );
    assert_eq!(
        run("var o={m(){return typeof new.target}}; o.m()"),
        "undefined"
    );
}
#[test]
fn catch_dup_binding() {
    assert!(Engine::new().eval("try{}catch([e,e]){}", false).is_err());
    assert!(Engine::new()
        .eval("try{}catch({a:x,b:x}){}", false)
        .is_err());
    assert_eq!(run("try{throw [1,2]}catch([a,b]){} 'ok'"), "ok");
    assert_eq!(run("try{throw 5}catch(e){} 'ok'"), "ok");
}
#[test]
fn delete_private_member() {
    assert!(Engine::new()
        .eval("class C{ #x=1; m(){ delete this.#x } }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C{ #x=1; m(){ delete this?.#x } }", false)
        .is_err());
    assert_eq!(
        run("class C{ #x=1; m(){ return delete this.foo } }; new C().m()"),
        "true"
    );
    assert_eq!(run("var o={a:1}; delete o.a; typeof o.a"), "undefined");
}
#[test]
fn class_validation() {
    assert!(Engine::new()
        .eval("class C{ #constructor(){} }", false)
        .is_err());
    assert!(Engine::new().eval("class C{ #x; #x; }", false).is_err());
    assert!(Engine::new()
        .eval("class C{ #x(){} #x(){} }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C{ constructor(){} constructor(){} }", false)
        .is_err());
    assert_eq!(
        run("class C{ get #x(){return 1} set #x(v){} m(){return this.#x} }; new C().m()"),
        "1"
    ); // get/set pair ok
    assert_eq!(
        run("class C{ #x=1; #y=2; s(){return this.#x+this.#y} }; new C().s()"),
        "3"
    );
    assert_eq!(
        run("class C{ static #s=5; static g(){return C.#s} }; C.g()"),
        "5"
    );
    // A private name occupies one slot for the whole class: instance + static `#x` is a duplicate.
    assert!(Engine::new()
        .eval("class C{ #x=1; static #x=2; }", false)
        .is_err());
}
#[test]
fn dstr_target_validation() {
    assert!(Engine::new().eval("({a:1}=2)", false).is_err());
    assert!(Engine::new().eval("[1]=2", false).is_err());
    assert!(Engine::new().eval("[a,1]=[]", false).is_err());
    assert_eq!(run("var a,b; ({a,b}={a:1,b:2}); a+','+b"), "1,2");
    assert_eq!(run("var a,b; [a,b]=[3,4]; a+','+b"), "3,4");
    assert_eq!(run("var o={}; ({a:o.x}={a:5}); o.x"), "5");
    assert_eq!(run("var a,b; ({a=1,b=2}={a:9}); a+','+b"), "9,2");
}
#[test]
fn regex_property_escapes() {
    assert_eq!(run(r"/\p{L}/u.test('A')"), "true");
    assert_eq!(run(r"/\p{L}/u.test('3')"), "false");
    assert_eq!(run(r"/\P{L}/u.test('3')"), "true");
    assert_eq!(run(r"/\p{Nd}/u.test('7')"), "true");
    assert_eq!(run(r"/\p{Script=Greek}/u.test('α')"), "true");
    assert_eq!(run(r"/\p{Script=Greek}/u.test('a')"), "false");
    assert_eq!(run(r"/\p{sc=Grek}/u.test('α')"), "true");
    assert_eq!(run(r"/\p{White_Space}/u.test(' ')"), "true");
    assert_eq!(run(r"/[\p{L}\p{N}]/u.test('5')"), "true");
    assert_eq!(run(r"/[^\p{L}]/u.test('A')"), "false");
    assert_eq!(run(r"/\p{Alphabetic}/u.test('A')"), "true");
    // invalid property -> parse-phase SyntaxError
    assert!(Engine::new().eval(r"/\p{Bogus}/u", false).is_err());
    // without u flag, \p is identity 'p'
    assert_eq!(run(r"/\p/.test('p')"), "true");
}
#[test]
fn regex_literal_parse_validation() {
    // invalid regex literals are now parse-phase SyntaxErrors
    assert!(Engine::new().eval(r"/\p{Bogus}/u", false).is_err());
    assert!(Engine::new().eval("/(?<a>)(?<a>)/", false).is_err());
    assert!(Engine::new().eval("/[z-a]/", false).is_err());
    assert!(Engine::new().eval("/a**/", false).is_err());
    assert_eq!(run(r"/\p{L}+/u.test('abc')"), "true");
    assert_eq!(run("/a+/.test('aaa')"), "true");
}
#[test]
fn unicode_identifiers() {
    // ID_Start / ID_Continue per the bundled UCD tables
    assert_eq!(run("var \u{00C5}=1; \u{00C5}"), "1"); // Å (Lu, ID_Start)
    assert_eq!(run("var \u{03B1}\u{03B2}=2; \u{03B1}\u{03B2}"), "2"); // αβ (Greek)
    assert_eq!(run("var _\u{0300}=3; _\u{0300}"), "3"); // _ + combining mark (ID_Continue)
    assert_eq!(run("var $x=4; $x"), "4");
    assert_eq!(run("var \u{4E2D}\u{6587}=5; \u{4E2D}\u{6587}"), "5"); // CJK
                                                                      // a lone combining mark can't START an identifier
    assert!(Engine::new().eval("var \u{0300}x=1", false).is_err());
    // ZWNJ/ZWJ valid as ID_Continue
    assert_eq!(run("var a\u{200D}b=6; a\u{200D}b"), "6");
}
#[test]
fn escaped_reserved_words() {
    // an escaped reserved word as a binding/identifier -> SyntaxError
    assert!(Engine::new().eval("var \\u0062reak = 1", false).is_err()); // break = break
    assert!(Engine::new().eval("\\u0062reak;", false).is_err());
    assert!(Engine::new().eval("var \\u{63}atch = 1", false).is_err()); // catch
                                                                        // but still valid as a property name
    assert_eq!(run("var o={break:1}; o.\\u0062reak"), "1");
    assert_eq!(run("var o={x:5}; o.return=9; o.return"), "9");
    // a normal escaped identifier is fine
    assert_eq!(run("var \\u0041bc = 7; Abc"), "7");
}
#[test]
fn named_backreferences() {
    assert_eq!(run(r"/(?<a>x)\k<a>/u.test('xx')"), "true");
    assert_eq!(run(r"/(?<a>x)\k<a>/u.test('xy')"), "false");
    assert_eq!(run(r"/\k<a>(?<a>x)/u.source"), r"\k<a>(?<a>x)"); // forward ref compiles
    assert_eq!(run(r"'abcabc'.replace(/(?<g>abc)\k<g>/, 'Z')"), "Z");
    // undefined named backref -> SyntaxError
    assert!(Engine::new().eval(r"/(?<a>x)\k<b>/u", false).is_err());
    assert!(Engine::new().eval(r"/\k<a>/u", false).is_err());
    // non-unicode, no named groups: \k is literal 'k'
    assert_eq!(run(r"/\k/.test('k')"), "true");
}
#[test]
fn catch_param_lexical_redecl() {
    assert!(Engine::new()
        .eval("try{}catch(e){ let e; }", false)
        .is_err());
    assert!(Engine::new()
        .eval("try{}catch(e){ const e=1; }", false)
        .is_err());
    assert!(Engine::new()
        .eval("try{}catch([a,b]){ let b; }", false)
        .is_err());
    assert!(Engine::new()
        .eval("try{}catch(e){ class e{} }", false)
        .is_err());
    // var of the same name is allowed (Annex B.3.4)
    assert_eq!(run("try{throw 1}catch(e){ var e = 2; } 'ok'"), "ok");
    // a different lexical name is fine
    assert_eq!(run("try{throw 1}catch(e){ let f = 2; } 'ok'"), "ok");
}
#[test]
fn numeric_separators() {
    let bad = [
        "1_", "1__2", "1_.5", "1._5", "0x_1", "0x1_", "1_e5", "1e_5", "1e5_", "0_1", "0b_1",
        "0b1_", "1_n", "123_",
    ];
    for src in bad {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "{src} should be invalid"
        );
    }
    assert_eq!(run("1_000"), "1000");
    assert_eq!(run("0x1_0"), "16");
    assert_eq!(run("1_0.0_1"), "10.01");
    assert_eq!(run("1_0e1_0"), "100000000000");
    assert_eq!(run("0b1_0"), "2");
    assert_eq!(run("123_456n"), "123456");
}
#[test]
fn var_nested_block_redecl() {
    assert!(Engine::new().eval("{ let x; { var x; } }", false).is_err());
    assert!(Engine::new()
        .eval("{ const x=1; { { var x; } } }", false)
        .is_err());
    assert!(Engine::new().eval("let y; { var y; }", false).is_err());
    // a var in a nested FUNCTION doesn't conflict with the outer let
    assert_eq!(
        run("{ let x=1; (function(){ var x=2; return x; }); x }"),
        "1"
    );
    // same-scope var-then-let still caught
    assert!(Engine::new().eval("{ var z; let z; }", false).is_err());
    // unrelated names fine
    assert_eq!(run("{ let a=1; { var b=2; } a }"), "1");
}
#[test]
fn shorthand_reserved_word() {
    assert!(Engine::new().eval("({ break } = {})", false).is_err());
    assert!(Engine::new().eval("var {break} = {}", false).is_err());
    assert!(Engine::new()
        .eval("var x = { bre\\u0061k } = { break: 42 };", false)
        .is_err());
    assert!(Engine::new().eval("({ null } = {})", false).is_err());
    // valid shorthand + keyword-named property with value are fine
    assert_eq!(run("var {x} = {x:5}; x"), "5");
    assert_eq!(run("var o={break:1}; o.break"), "1");
    assert_eq!(run("var {break:b} = {break:7}; b"), "7");
}
#[test]
fn private_name_no_escape() {
    // the '#' of a private name can't be a unicode escape
    assert!(Engine::new()
        .eval("class C { \\u0023x = 1 }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C { #x=1; m(){ return this.\\u0023x } }", false)
        .is_err());
    // a leading combining mark / ZWJ via escape can't start an identifier
    assert!(Engine::new().eval("var \\u0300x = 1", false).is_err());
    assert!(Engine::new().eval("var \\u200Dx = 1", false).is_err());
    // but escaping the NAME part of a private field (not the #) is fine
    assert_eq!(
        run("class C { #x=5; m(){ return this.#\\u0078 } }; new C().m()"),
        "5"
    );
    assert_eq!(run("var \\u0041bc = 7; Abc"), "7");
}
#[test]
fn undeclared_private_name() {
    assert!(Engine::new()
        .eval("class C { m() { something.#x } }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C { m() { return this.#y } }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C { #x=1; m() { return obj.#z } }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C { m() { return #w in obj } }", false)
        .is_err());
    assert!(Engine::new().eval("obj.#top", false).is_err()); // outside any class
                                                             // valid: declared in the class (incl. forward + nested-class enclosing)
    assert_eq!(
        run("class C { #x=5; getX(){return this.#x} }; new C().getX()"),
        "5"
    );
    assert_eq!(
        run("class C { useLater(){return this.#y} #y=7 }; new C().useLater()"),
        "7"
    );
    assert_eq!(
        run("class C { #x=1; m(){ return class D { d(o){ return o.#x } } } } typeof new C().m()"),
        "function"
    );
    assert_eq!(
        run("class C { #x=3; has(o){ return #x in o } }; var c=new C(); c.has(c)"),
        "true"
    );
}
#[test]
fn nonsimple_params_use_strict() {
    let bad = [
        "function f(a=1){'use strict'}",
        "function f([a]){'use strict'}",
        "function f(...a){'use strict'}",
        "var f=(a=1)=>{'use strict'}",
        "var o={m(a=1){'use strict'}}",
        "var o={*m([a]){'use strict'}}",
        "async function f(a=1){'use strict'}",
        "class C{m(...a){'use strict'}}",
        "var o={async *m(a=1){'use strict'}}",
    ];
    for src in bad {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "{src} should be invalid"
        );
    }
    // simple params + use strict are fine
    assert_eq!(run("function f(a){'use strict'; return a} f(5)"), "5");
    assert_eq!(run("var o={m(){'use strict'; return 9}}; o.m()"), "9");
    // non-simple params WITHOUT a use-strict directive are fine
    assert_eq!(run("function f(a=3){return a} f()"), "3");
}
#[test]
fn new_import_error() {
    assert!(Engine::new().eval("new import('x')", false).is_err());
    assert!(Engine::new().eval("()=>new import('x')", false).is_err());
    assert!(Engine::new().eval("new import.meta", false).is_err()); // import.meta in script also errors
                                                                    // normal new still works
    assert_eq!(run("function F(){this.x=1} new F().x"), "1");
}
#[test]
fn block_async_fn_redecl() {
    assert!(Engine::new()
        .eval("{ async function f(){} async function f(){} }", false)
        .is_err());
    assert!(Engine::new()
        .eval("{ async function f(){} function f(){} }", false)
        .is_err());
    assert!(Engine::new()
        .eval("{ function* g(){} function* g(){} }", false)
        .is_err());
    assert!(Engine::new()
        .eval("{ async function f(){} var f; }", false)
        .is_err());
    assert!(Engine::new()
        .eval(
            "switch(0){ case 1: async function f(){} default: function f(){} }",
            false
        )
        .is_err());
    // plain function redeclaration in a block is still allowed (Annex B)
    assert_eq!(
        run("{ function f(){return 1} function f(){return 2} } 'ok'"),
        "ok"
    );
    // async function redeclaration at TOP level is allowed
    assert_eq!(run("async function f(){} async function f(){} 'ok'"), "ok");
}
#[test]
fn new_import_nested() {
    assert!(Engine::new().eval("new import('')", false).is_err());
    assert!(Engine::new().eval("new import('').then()", false).is_err());
    assert!(Engine::new().eval("new import('').foo", false).is_err());
    assert!(Engine::new()
        .eval("() => new import('').then()", false)
        .is_err());
    // legitimate: new on a call result is fine
    assert_eq!(
        run("function mk(){ return function(){this.x=4} } new (mk())().x"),
        "4"
    );
    assert_eq!(run("function F(){this.y=2} new F().y"), "2");
}
#[test]
fn regex_group_name_validation() {
    assert!(Engine::new().eval("/(?<>x)/u", false).is_err()); // empty
    assert!(Engine::new().eval("/(?<1a>x)/u", false).is_err()); // starts with digit
    assert!(Engine::new().eval("/(?<a b>x)/u", false).is_err()); // space
    assert!(Engine::new().eval("/(?<a.b>x)/u", false).is_err()); // dot
                                                                 // valid names
    assert_eq!(run(r"/(?<a>x)/u.test('x')"), "true");
    assert_eq!(run(r"/(?<$_a1>x)/u.test('x')"), "true");
    assert_eq!(run("/(?<\\u0061b>x)/u.test('x')"), "true"); // escaped 'a'
    assert_eq!(run(r"/(?<café>x)/u.test('x')"), "true"); // unicode
}
#[test]
fn regex_no_line_terminator() {
    assert!(Engine::new().eval("/\\\n/", false).is_err()); // backslash + LF
    assert!(Engine::new().eval("/a\nb/", false).is_err()); // raw LF in body
    assert!(Engine::new().eval("/[\\\n]/", false).is_err()); // backslash+LF in class
    assert_eq!(run(r"/\n/.test('\n')"), "true"); // \n escape (valid)
    assert_eq!(run(r"/ab/.test('ab')"), "true");
}
#[test]
fn private_names_not_observable() {
    assert_eq!(
        run("class C{ static #x(){return 1} } Object.prototype.hasOwnProperty.call(C,'#x')"),
        "false"
    );
    assert_eq!(
        run("class C{ #f=1 } var c=new C(); c.hasOwnProperty('#f')"),
        "false"
    );
    assert_eq!(run("class C{ #f=1; m(){return this.#f} } var c=new C(); Object.getOwnPropertyNames(c).length"), "0");
    assert_eq!(
        run("class C{ #f=1 } var c=new C(); Object.keys(c).join(',')"),
        ""
    );
    assert_eq!(
        run("class C{ #f=1 } var c=new C(); Object.getOwnPropertyDescriptor(c,'#f')"),
        "undefined"
    );
    assert_eq!(
        run("class C{ #f=1; m(){var s=''; for(var k in this)s+=k; return s} } new C().m()"),
        ""
    );
    // private access still works
    assert_eq!(
        run("class C{ #f=5; get(){return this.#f} } new C().get()"),
        "5"
    );
    assert_eq!(
        run("class C{ #m(){return 9}; call(){return this.#m()} } new C().call()"),
        "9"
    );
    // normal props still enumerable
    assert_eq!(
        run("class C{ a=1 } var c=new C(); Object.keys(c).join(',')"),
        "a"
    );
}
#[test]
fn ta_meta_not_own() {
    assert_eq!(
        run("Object.getOwnPropertyNames(new Int8Array(2)).join(',')"),
        "0,1"
    );
    assert_eq!(
        run("new Int8Array(2).hasOwnProperty('byteLength')"),
        "false"
    );
    assert_eq!(run("new Int8Array(2).hasOwnProperty('buffer')"), "false");
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(new Int8Array(2),'length')"),
        "undefined"
    );
    // meta still readable (inherited/computed)
    assert_eq!(run("new Int32Array(4).length"), "4");
    assert_eq!(run("new Int32Array(4).byteLength"), "16");
    assert_eq!(run("new Float64Array(3).BYTES_PER_ELEMENT"), "8");
    assert_eq!(
        run("var b=new ArrayBuffer(8); new Int8Array(b).buffer===b"),
        "true"
    );
    assert_eq!(
        run("var a=new Int8Array(new ArrayBuffer(8),2,3); a.byteOffset"),
        "2"
    );
}
#[test]
fn ta_prototype_accessors() {
    // the accessors exist on %TypedArray.prototype% and brand-check
    assert_eq!(run("var p=Object.getPrototypeOf(Int8Array.prototype); typeof Object.getOwnPropertyDescriptor(p,'byteLength').get"), "function");
    assert_eq!(run("var g=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Int8Array.prototype),'length').get; try{g.call({});'no'}catch(e){e.constructor.name}"), "TypeError");
    assert_eq!(run("var g=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Uint8Array.prototype),'byteOffset').get; g.call(new Uint8Array(new ArrayBuffer(8),2,3))"), "2");
    // normal instance reads still work
    assert_eq!(run("new Float64Array(3).byteLength"), "24");
    assert_eq!(
        run("var b=new ArrayBuffer(4); new Int8Array(b).buffer===b"),
        "true"
    );
}
#[test]
fn number_tostring_spec() {
    let cases = [
        ("1e21", "1e+21"),
        ("1e-7", "1e-7"),
        ("1e20", "100000000000000000000"),
        ("0.0000001", "1e-7"),
        ("1e100", "1e+100"),
        ("5e-324", "5e-324"),
        ("1.7976931348623157e308", "1.7976931348623157e+308"),
        ("0.1", "0.1"),
        ("100", "100"),
        ("1.5", "1.5"),
        ("-0", "0"),
        ("-2.5", "-2.5"),
        ("1e-6", "0.000001"),
        ("123.456", "123.456"),
        ("0.000001", "0.000001"),
        ("12345678900000000000", "12345678900000000000"),
        ("255", "255"),
        ("1000000000000000128", "1000000000000000100"),
    ];
    for (src, want) in cases {
        assert_eq!(run(&format!("({src})+''")), want, "({src})+''");
    }
}
#[test]
fn number_methods_fixed() {
    let cases = [
        ("(123.456).toFixed(2)", "123.46"),
        ("(0).toFixed(2)", "0.00"),
        ("(1e21).toFixed(2)", "1e+21"),
        ("(-0).toFixed(0)", "0"),
        ("(-1.5).toFixed(0)", "-2"),
        ("(123.456).toPrecision(4)", "123.5"),
        ("(12345).toPrecision(2)", "1.2e+4"),
        ("(0.0001).toPrecision(1)", "0.0001"),
        ("(5).toPrecision(1)", "5"),
        ("(0).toPrecision(3)", "0.00"),
        ("(123.456).toPrecision()", "123.456"),
        ("(1).toPrecision(5)", "1.0000"),
        ("(255).toString(16)", "ff"),
        ("(123.456).toExponential(2)", "1.23e+2"),
    ];
    for (src, want) in cases {
        assert_eq!(run(src), want, "{src}");
    }
}
#[test]
fn shadow_realm_basic() {
    assert_eq!(run("typeof ShadowRealm"), "function");
    assert_eq!(run("typeof ShadowRealm.prototype.evaluate"), "function");
    assert_eq!(run("var r=new ShadowRealm(); r.evaluate('1+1')"), "2");
    assert_eq!(run("var r=new ShadowRealm(); r.evaluate('null')"), "null");
    assert_eq!(
        run("var r=new ShadowRealm(); typeof r.evaluate('undefined')"),
        "undefined"
    );
    assert_eq!(run("var r=new ShadowRealm(); r.evaluate('\"str\"')"), "str");
    assert_eq!(
        run("var r=new ShadowRealm(); typeof r.evaluate('function fn(){}')"),
        "undefined"
    );
    // isolation: the shadow realm has its own globals
    assert_eq!(
        run("var r=new ShadowRealm(); globalThis.x=5; typeof r.evaluate('typeof x')"),
        "string"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); r.evaluate('typeof x')"),
        "undefined"
    );
    // errors: non-string arg, bad syntax, thrown error
    assert_eq!(
        run("var r=new ShadowRealm(); try{r.evaluate(1)}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); try{r.evaluate('(')}catch(e){e.constructor.name}"),
        "SyntaxError"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); try{r.evaluate('throw 1')}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); try{r.evaluate('({})')}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("try{ShadowRealm()}catch(e){e.constructor.name}"),
        "TypeError"
    );
}
#[test]
fn shadow_realm_wrapped_fn() {
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('x=>x+1'); typeof f"),
        "function"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('x=>x*2'); f(21)"),
        "42"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('(a,b)=>a+b'); f(3,4)"),
        "7"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('()=>\"hi\"'); f()"),
        "hi"
    );
    // a wrapped function isn't constructable, and passing an object throws
    assert_eq!(run("var r=new ShadowRealm(); var f=r.evaluate('x=>x'); try{f({})}catch(e){e.constructor.name}"), "TypeError");
    // returned function from a wrapped call is itself wrapped
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('a=>b=>a+b'); typeof f(1)"),
        "function"
    );
}
#[test]
fn array_exotic_defineprop() {
    assert!(Engine::new()
        .eval("Object.defineProperty([],'length',{value:-1})", false)
        .map(|c| matches!(c,Completion::Throw{ref name,..} if name=="RangeError"))
        .unwrap_or(false));
    assert!(Engine::new()
        .eval(
            "Object.defineProperty([],'length',{value:4294967296})",
            false
        )
        .map(|c| matches!(c,Completion::Throw{ref name,..} if name=="RangeError"))
        .unwrap_or(false));
    assert!(Engine::new()
        .eval("Object.defineProperty([],'length',{value:1.5})", false)
        .map(|c| matches!(c,Completion::Throw{ref name,..} if name=="RangeError"))
        .unwrap_or(false));
    // truncation deletes elements
    assert_eq!(
        run("var a=[1,2,3]; Object.defineProperty(a,'length',{value:1}); a.length+','+(1 in a)"),
        "1,false"
    );
    // defining an index past length grows length
    assert_eq!(run("var a=[1]; Object.defineProperty(a,'5',{value:9,writable:true,enumerable:true,configurable:true}); a.length"), "6");
    // non-writable length blocks index growth
    assert_eq!(run("var a=[1]; Object.defineProperty(a,'length',{writable:false}); var ok=true; try{Object.defineProperty(a,'5',{value:9})}catch(e){} a.length"), "1");
    // valid length set works
    assert_eq!(
        run("var a=[1,2]; Object.defineProperty(a,'length',{value:5}); a.length"),
        "5"
    );
}
#[test]
fn regex_prop_syntax() {
    // spaces in \p{} are invalid
    assert!(Engine::new()
        .eval(r"/\p{ General_Category=Letter }/u", false)
        .is_err());
    assert!(Engine::new().eval(r"/\p{Letter }/u", false).is_err());
    // class escape as a range bound (unicode) is invalid
    assert!(Engine::new().eval(r"/[--\p{Hex}]/u", false).is_err());
    assert!(Engine::new().eval(r"/[\d-a]/u", false).is_err());
    assert!(Engine::new().eval(r"/[\p{L}-\p{N}]/u", false).is_err());
    // valid forms still work
    assert_eq!(run(r"/\p{Letter}/u.test('a')"), "true");
    assert_eq!(run(r"/\p{General_Category=Letter}/u.test('a')"), "true");
    assert_eq!(run(r"/[a-z]/u.test('m')"), "true");
    assert_eq!(run(r"/[\d]/.test('5')"), "true");
    assert_eq!(run(r"/[\d-a]/.test('-')"), "true"); // non-unicode: lenient
}
#[test]
fn regex_inline_modifiers() {
    assert_eq!(run(r"/(?i:a)b/.test('Ab')"), "true");
    assert_eq!(run(r"/(?i:a)b/.test('AB')"), "false"); // b stays case-sensitive
    assert_eq!(run(r"/a(?i:b)c/.test('aBc')"), "true");
    assert_eq!(run(r"/(?-i:a)/i.test('A')"), "false"); // remove i
    assert_eq!(run(r"/(?-i:a)b/i.test('aB')"), "true");
    assert_eq!(run(r"/(?m:^b)/.test('a\nb')"), "true");
    assert_eq!(run(r"/(?s:.)/.test('\n')"), "true");
    assert_eq!(run(r"/(?i:[a-z])/.test('Q')"), "true");
    // backtracking across the modifier boundary keeps flags correct
    assert_eq!(run(r"/(?i:a+)A/.test('AAA')"), "true");
    assert_eq!(run(r"/(?i:a+)a/.test('AAA')"), "false");
    // invalid modifiers
    assert!(Engine::new().eval(r"/(?z:a)/", false).is_err());
    assert!(Engine::new().eval(r"/(?-:a)/", false).is_err());
    assert!(Engine::new().eval(r"/(?ii:a)/", false).is_err());
}
#[test]
fn proxy_get_invariant() {
    assert!(
        matches!(Engine::new().eval("var t={};Object.defineProperty(t,'x',{value:1,writable:false,configurable:false});var p=new Proxy(t,{get(){return 2}});p.x", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert!(
        matches!(Engine::new().eval("var t={};Object.defineProperty(t,'x',{get:undefined,configurable:false});var p=new Proxy(t,{get(){return 2}});p.x", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    // returning the same value is fine
    assert_eq!(run("var t={};Object.defineProperty(t,'x',{value:1,writable:false,configurable:false});var p=new Proxy(t,{get(){return 1}});p.x"), "1");
    // configurable property: trap can return anything
    assert_eq!(
        run("var t={x:1};var p=new Proxy(t,{get(){return 9}});p.x"),
        "9"
    );
}
#[test]
fn proxy_set_invariant() {
    assert!(
        matches!(Engine::new().eval("var t={};Object.defineProperty(t,'x',{value:1,writable:false,configurable:false});var p=new Proxy(t,{set(){return true}});p.x=2", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert_eq!(
        run("var t={x:1};var p=new Proxy(t,{set(o,k,v){o[k]=v;return true}});p.x=5; t.x"),
        "5"
    );
}
#[test]
fn proxy_more_invariants() {
    assert!(
        matches!(Engine::new().eval("var t={};Object.defineProperty(t,'x',{value:1,configurable:false});var p=new Proxy(t,{has(){return false}});'x' in p", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert!(
        matches!(Engine::new().eval("var t={};Object.preventExtensions(t);var p=new Proxy(t,{isExtensible(){return true}});Object.isExtensible(p)", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    // valid cases
    assert_eq!(
        run("var t={x:1};var p=new Proxy(t,{has(){return true}});'y' in p"),
        "true"
    );
    assert_eq!(
        run("var p=new Proxy({},{isExtensible(){return true}});Object.isExtensible(p)"),
        "true"
    );
}
#[test]
fn object_methods_coerce() {
    assert_eq!(run("Object.keys('ab').join(',')"), "0,1");
    assert_eq!(run("Object.values('ab').join(',')"), "a,b");
    assert_eq!(run("Object.entries('ab').length"), "2");
    assert_eq!(
        run("Object.getOwnPropertyNames('ab').join(',')"),
        "0,1,length"
    );
    assert_eq!(run("Object.keys(5).length"), "0");
    assert!(
        matches!(Engine::new().eval("Object.keys(null)", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert!(
        matches!(Engine::new().eval("Object.values(undefined)", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    // normal objects still work
    assert_eq!(run("Object.keys({a:1,b:2}).join(',')"), "a,b");
}
#[test]
fn array_isarray_proxy() {
    assert_eq!(run("Array.isArray(new Proxy([],{}))"), "true");
    assert_eq!(run("Array.isArray(new Proxy(new Proxy([],{}),{}))"), "true");
    assert_eq!(run("Array.isArray(new Proxy({},{}))"), "false");
    assert_eq!(run("Array.isArray([])"), "true");
    assert_eq!(run("Array.isArray({})"), "false");
}
#[test]
fn arraybuffer_length_validation() {
    assert!(
        matches!(Engine::new().eval("new ArrayBuffer(-1)", false), Ok(Completion::Throw{ref name,..}) if name=="RangeError")
    );
    assert!(
        matches!(Engine::new().eval("new ArrayBuffer(Infinity)", false), Ok(Completion::Throw{ref name,..}) if name=="RangeError")
    );
    assert_eq!(run("new ArrayBuffer(NaN).byteLength"), "0");
    assert_eq!(run("new ArrayBuffer(8.9).byteLength"), "8");
    assert_eq!(run("new ArrayBuffer(8).byteLength"), "8");
}
#[test]
fn array_methods_coerce_primitive() {
    assert_eq!(run("Boolean.prototype[0]=true;Boolean.prototype.length=1;Array.prototype.lastIndexOf.call(true,true)"), "0");
    assert_eq!(run("Array.prototype.indexOf.call('abc','b')"), "1");
    assert_eq!(run("Array.prototype.join.call('abc','-')"), "a-b-c");
    assert_eq!(
        run("var s='';Array.prototype.forEach.call('ab',c=>s+=c);s"),
        "ab"
    );
    assert_eq!(
        run("Array.prototype.map.call('ab',c=>c.toUpperCase()).join('')"),
        "AB"
    );
    assert!(
        matches!(Engine::new().eval("Array.prototype.indexOf.call(null,1)", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
}
#[test]
fn array_concat_slice_holes() {
    assert_eq!(run("[1,,3].concat([4]).hasOwnProperty(1)"), "false");
    assert_eq!(run("[1,,3].slice().hasOwnProperty(1)"), "false");
    assert_eq!(run("[1,,3].concat([4]).length"), "4");
    assert_eq!(run("[1,2].concat(3,[4,5]).join(',')"), "1,2,3,4,5");
    // isConcatSpreadable
    assert_eq!(
        run("var o={length:2,0:'a',1:'b',[Symbol.isConcatSpreadable]:true};[].concat(o).join(',')"),
        "a,b"
    );
    assert_eq!(
        run("var a=[1,2];a[Symbol.isConcatSpreadable]=false;[].concat(a).length"),
        "1"
    );
    assert_eq!(run("[1,2,3].slice(1).join(',')"), "2,3");
}
#[test]
fn date_parse_rfc() {
    assert_eq!(run("Date.parse('Thu, 01 Jan 1970 00:00:00 GMT')"), "0");
    assert_eq!(run("Date.parse('Thu Jan 01 1970 00:00:00 GMT+0000')"), "0");
    assert_eq!(run("var d=new Date(Date.UTC(1993,6,28,14,39,7)); Date.parse(d.toUTCString())===d.getTime()-d.getMilliseconds()"), "true");
    assert_eq!(
        run("Date.parse('Mon, 25 Dec 1995 13:30:00 GMT')"),
        "819898200000"
    );
    assert_eq!(run("Date.parse('2020-01-01T00:00:00Z')"), "1577836800000"); // ISO still works
    assert_eq!(run("isNaN(Date.parse('garbage'))"), "true");
}
#[test]
fn date_get_set_year() {
    assert_eq!(run("new Date(Date.UTC(1970,0,1)).getYear()"), "70");
    assert_eq!(run("new Date(Date.UTC(2020,0,1)).getYear()"), "120");
    assert_eq!(
        run("var d=new Date(0); d.setYear(99); d.getFullYear()"),
        "1999"
    );
    assert_eq!(
        run("var d=new Date(0); d.setYear(2020); d.getFullYear()"),
        "2020"
    );
    assert_eq!(run("isNaN(new Date(NaN).getYear())"), "true");
    assert_eq!(run("typeof Date.prototype.getYear"), "function");
}
#[test]
fn promise_combinator_this_check() {
    for m in ["all", "race", "allSettled", "any"] {
        assert!(
            matches!(Engine::new().eval(&format!("Promise.{m}.call(undefined,[])"), false), Ok(Completion::Throw{ref name,..}) if name=="TypeError"),
            "{m} undefined"
        );
        assert!(
            matches!(Engine::new().eval(&format!("Promise.{m}.call({{}},[])"), false), Ok(Completion::Throw{ref name,..}) if name=="TypeError"),
            "{m} obj"
        );
        assert!(
            matches!(Engine::new().eval(&format!("Promise.{m}.call(()=>{{}},[])"), false), Ok(Completion::Throw{ref name,..}) if name=="TypeError"),
            "{m} arrow"
        );
    }
    // normal use still works (returns a promise)
    assert_eq!(run("typeof Promise.all([])"), "object");
    assert_eq!(run("typeof Promise.race([Promise.resolve(1)])"), "object");
}
#[test]
fn dataview_offset_validation() {
    assert!(
        matches!(Engine::new().eval("new DataView(new ArrayBuffer(8),-1)", false), Ok(Completion::Throw{ref name,..}) if name=="RangeError")
    );
    assert!(
        matches!(Engine::new().eval("new DataView(new ArrayBuffer(8),10)", false), Ok(Completion::Throw{ref name,..}) if name=="RangeError")
    );
    assert!(
        matches!(Engine::new().eval("new DataView(new ArrayBuffer(8),4,8)", false), Ok(Completion::Throw{ref name,..}) if name=="RangeError")
    );
    assert_eq!(run("new DataView(new ArrayBuffer(8),2).byteLength"), "6");
    assert_eq!(run("new DataView(new ArrayBuffer(8),2,4).byteLength"), "4");
    assert_eq!(run("new DataView(new ArrayBuffer(8)).byteLength"), "8");
}
#[test]
fn loop_completion_values() {
    assert_eq!(run("for(var i=0;i<3;i++){ i }"), "2");
    assert_eq!(run("2; for(var i=0;i<0;i++){ 3 }"), "2"); // no iteration → empty → keeps 2
    assert_eq!(run("for(var i=0;i<3;i++){ }"), "undefined");
    assert_eq!(run("var i=0; while(i<3){ i++; i }"), "3");
    assert_eq!(run("var i=0; do { i++; i } while(i<3)"), "3");
    assert_eq!(run("for(var k of [10,20,30]){ k }"), "30");
    assert_eq!(run("for(var k in {a:1,b:2}){ k }"), "b");
    assert_eq!(run("for(var i=0;i<3;i++){ continue; 99 }"), "undefined");
}
#[test]
fn fn_decl_stmt_position() {
    // always SyntaxError
    assert!(Engine::new()
        .eval("if(true) async function f(){}", false)
        .is_err());
    assert!(Engine::new()
        .eval("if(true) function* f(){}", false)
        .is_err());
    assert!(Engine::new()
        .eval("while(false) function f(){}", false)
        .is_err());
    assert!(Engine::new().eval("for(;;) function f(){}", false).is_err());
    assert!(Engine::new()
        .eval("do function f(){} while(false)", false)
        .is_err());
    assert!(Engine::new().eval("x: function* f(){}", false).is_err());
    assert!(Engine::new()
        .eval("x: async function f(){}", false)
        .is_err());
    // Annex B sloppy: plain function as if/else/label body is OK
    assert!(Engine::new().eval("if(true) function f(){}", false).is_ok());
    assert!(Engine::new()
        .eval("if(0); else function f(){}", false)
        .is_ok());
    assert!(Engine::new().eval("x: function f(){}", false).is_ok());
    // strict: not allowed
    assert!(Engine::new()
        .eval("'use strict'; if(true) function f(){}", false)
        .is_err());
    // normal block declarations still fine
    assert_eq!(run("{ function f(){return 5} } f()"), "5");
    assert_eq!(run("if(true){ function g(){return 7} } g()"), "7");
}
#[test]
fn regex_prop_invalid_special() {
    for pat in [
        r"/\p{ANY}/u",
        r"/\p{any}/u",
        r"/\p{ASSIGNED}/u",
        r"/\p{assigned}/u",
        r"/\p{Ascii}/u",
        r"/\p{ascii}/u",
    ] {
        assert!(
            Engine::new().eval(pat, false).is_err(),
            "{pat} should be SyntaxError"
        );
    }
    // valid ones still work
    assert_eq!(run(r"/\p{ASCII_Hex_Digit}/u.test('F')"), "true");
    assert_eq!(run(r"/\p{Lowercase}/u.test('a')"), "true");
}
#[test]
fn sort_comparator_validation() {
    assert!(
        matches!(Engine::new().eval("[1,2].sort('x')", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert!(
        matches!(Engine::new().eval("[1,2].sort(5)", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert!(
        matches!(Engine::new().eval("[1,2].sort({})", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert_eq!(run("[3,1,2].sort().join(',')"), "1,2,3");
    assert_eq!(run("[3,1,2].sort((a,b)=>a-b).join(',')"), "1,2,3");
    assert_eq!(run("[3,1,2].sort(undefined).join(',')"), "1,2,3");
}
#[test]
fn string_replace_all_regex() {
    assert_eq!(run("'aaa'.replaceAll(/a/g,'b')"), "bbb");
    assert_eq!(run("'a1b2c3'.replaceAll(/\\d/g,'_')"), "a_b_c_");
    assert!(
        matches!(Engine::new().eval("'a'.replaceAll(/a/,'b')", false), Ok(Completion::Throw{ref name,..}) if name=="TypeError")
    );
    assert_eq!(run("'aaa'.replaceAll('a','b')"), "bbb"); // string path still works
    assert_eq!(run("'a1a2'.replaceAll(/a(\\d)/g,'[$1]')"), "[1][2]");
}
#[test]
fn error_cause() {
    assert_eq!(run("new Error('m',{cause:42}).cause"), "42");
    assert_eq!(run("'cause' in new Error('m')"), "false");
    assert_eq!(run("new TypeError('x',{cause:'y'}).cause"), "y");
    assert_eq!(run("new AggregateError([],'m',{cause:9}).cause"), "9");
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(new Error('m',{cause:1}),'cause').enumerable"),
        "false"
    );
    assert_eq!(run("new Error('m',{}).hasOwnProperty('cause')"), "false");
    assert_eq!(run("new Error('m', {cause: undefined}).cause"), "undefined");
    assert_eq!(
        run("new Error('m', {cause: undefined}).hasOwnProperty('cause')"),
        "true"
    );
}
#[test]
fn sloppy_this_boxing() {
    assert_eq!(
        run("function f(){return eval('this')}f.call(42) instanceof Number"),
        "true"
    );
    assert_eq!(
        run("function f(){return this}; typeof f.call('hi')"),
        "object"
    );
    assert_eq!(run("function f(){return this.valueOf()}; f.call(5)"), "5");
    // strict mode: primitive this stays primitive
    assert_eq!(
        run("function f(){'use strict';return typeof this}; f.call(5)"),
        "number"
    );
    // object this passes through
    assert_eq!(
        run("var o={};function f(){return this===o}; f.call(o)"),
        "true"
    );
}
#[test]
fn generator_coroutine() {
    // lazy: body doesn't run until next()
    assert_eq!(
        run("var log='';function* g(){log+='a';yield 1;log+='b';yield 2}var it=g();log"),
        ""
    );
    assert_eq!(
        run("function* g(){yield 1;yield 2}var it=g();it.next().value+','+it.next().value"),
        "1,2"
    );
    assert_eq!(
        run("function* g(){yield 1}var it=g();it.next();it.next().done"),
        "true"
    );
    // yield expression value injection
    assert_eq!(
        run("function* g(){var x=yield 1;yield x}var it=g();it.next();it.next(10).value"),
        "10"
    );
    // return value
    assert_eq!(run("function* g(){yield 1;return 9}var it=g();it.next();var r=it.next();r.value+','+r.done"), "9,true");
    // return() method
    assert_eq!(
        run("function* g(){yield 1;yield 2}var it=g();it.next();it.return(5).value"),
        "5"
    );
    // throw() into a try/catch
    assert_eq!(
        run("function* g(){try{yield 1}catch(e){yield e}}var it=g();it.next();it.throw('X').value"),
        "X"
    );
    // yield* delegation
    assert_eq!(
        run("function* a(){yield 1;yield 2}function* g(){yield* a();yield 3}[...g()].join(',')"),
        "1,2,3"
    );
    // spread + for-of
    assert_eq!(run("function* g(){yield 1;yield 2}[...g()].length"), "2");
    assert_eq!(
        run("var s=0;function* g(){yield 1;yield 2;yield 3}for(var x of g())s+=x;s"),
        "6"
    );
    // infinite generator, taken lazily
    assert_eq!(run("function* nat(){var i=0;while(true)yield i++}var it=nat();it.next();it.next();it.next().value"), "2");
    // side-effect ordering
    assert_eq!(run("var log='';function* g(){log+='1';yield;log+='2';yield;log+='3'}var it=g();it.next();it.next();log"), "12");
}
#[test]
fn async_coroutine() {
    // helper: run setup (drains microtasks), then read an expression
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    assert_eq!(
        two("globalThis.r=0;(async()=>{globalThis.r=await 5})()", "r"),
        "5"
    );
    assert_eq!(two("globalThis.r='';(async()=>{globalThis.r+='a';await 0;globalThis.r+='b'})();globalThis.r+='c'", "r"), "acb"); // await suspends after 'a', 'c' runs sync, then 'b'
    assert_eq!(
        two(
            "globalThis.r=0;async function f(){return 7}f().then(v=>globalThis.r=v)",
            "r"
        ),
        "7"
    );
    assert_eq!(
        two(
            "globalThis.r=0;async function f(){throw 9}f().catch(e=>globalThis.r=e)",
            "r"
        ),
        "9"
    );
    assert_eq!(two("globalThis.r=0;async function f(){var x=await 1;var y=await 2;return x+y}f().then(v=>globalThis.r=v)", "r"), "3");
    assert_eq!(two("globalThis.r=0;async function f(){try{await Promise.reject(8)}catch(e){return e+1}}f().then(v=>globalThis.r=v)", "r"), "9");
    assert_eq!(
        two(
            "globalThis.r='';async function f(){for(var i=0;i<3;i++){await 0;globalThis.r+=i}}f()",
            "r"
        ),
        "012"
    );
    assert_eq!(two("globalThis.r=0;async function f(){return await Promise.resolve(42)}f().then(v=>globalThis.r=v)", "r"), "42");
}
#[test]
fn async_generator_coroutine() {
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    // async generator yields, consumed via for-await collected into a global
    assert_eq!(two("globalThis.r='';async function* g(){yield 1;yield 2;yield 3}(async()=>{for await(const x of g())globalThis.r+=x})()", "r"), "123");
    // await inside async generator
    assert_eq!(two("globalThis.r='';async function* g(){yield await Promise.resolve('a');yield 'b'}(async()=>{for await(const x of g())globalThis.r+=x})()", "r"), "ab");
    // next() returns a promise of {value,done}
    assert_eq!(two("globalThis.r=0;async function* g(){yield 5}g().next().then(o=>globalThis.r=o.value+(o.done?'D':'N'))", "r"), "5N");
    assert_eq!(
        two(
            "globalThis.r=0;async function* g(){}g().next().then(o=>globalThis.r=(o.done?'D':'N'))",
            "r"
        ),
        "D"
    );
}

#[test]
fn decorators_runtime() {
    // Method decorator replaces the method.
    assert_eq!(
        run(r#"
            function double(fn, ctx) { return function(...a){ return fn.apply(this,a)*2; }; }
            class C { @double m(){ return 5; } }
            String(new C().m())
        "#),
        "10"
    );
    // Context shape for a method decorator.
    assert_eq!(
        run(r#"
            let info;
            function probe(fn, ctx){ info = ctx.kind+","+ctx.name+","+ctx.static+","+ctx.private; }
            class C { @probe static foo(){} }
            info
        "#),
        "method,foo,true,false"
    );
    // Field decorator initializer transforms the value.
    assert_eq!(
        run(r#"
            function plus1(v, ctx){ return function(init){ return init + 1; }; }
            class C { @plus1 x = 10; }
            String(new C().x)
        "#),
        "11"
    );
    // addInitializer runs with this = instance.
    assert_eq!(
        run(r#"
            function init(v, ctx){ ctx.addInitializer(function(){ this.ran = true; }); }
            class C { @init m(){} }
            String(new C().ran)
        "#),
        "true"
    );
    // Class decorator replaces the class.
    assert_eq!(
        run(r#"
            function tag(cls, ctx){ cls.tagged = ctx.name; return cls; }
            @tag class C {}
            C.tagged
        "#),
        "C"
    );
    // Accessor decorator can wrap get and add init.
    assert_eq!(
        run(r#"
            function dec(t, ctx){
                return { get(){ return t.get.call(this) + 100; }, init(v){ return 5; } };
            }
            class C { @dec accessor x = 1; }
            String(new C().x)
        "#),
        "105"
    );
}

#[test]
fn string_search_position_and_regexp() {
    // includes/startsWith/endsWith honor the position argument.
    assert_eq!(run("'word'.includes('o', 3)"), "false");
    assert_eq!(run("'word'.includes('d', 3)"), "true");
    assert_eq!(run("'abcabc'.startsWith('abc', 3)"), "true");
    assert_eq!(run("'abcabc'.startsWith('abc', 1)"), "false");
    assert_eq!(run("'hello'.endsWith('ell', 4)"), "true");
    // true coerces to position 1.
    assert_eq!(run("'word'.includes('w', true)"), "false");
    // A RegExp search argument is a TypeError.
    assert_eq!(throws("'abc'.includes(/a/)"), "TypeError");
    assert_eq!(throws("'abc'.startsWith(/a/)"), "TypeError");
    // indexOf honors the position.
    assert_eq!(run("'ABABAB'.indexOf('AB', 1)"), "2");
    assert_eq!(run("'abc'.indexOf('', 2)"), "2");
}

#[test]
fn string_trim_feff() {
    // U+FEFF (ZWNBSP) is whitespace for trim and ToNumber.
    assert_eq!(run("'\\uFEFF abc \\uFEFF'.trim()"), "abc");
    assert_eq!(run("'\\uFEFF5'.trimStart()"), "5");
    assert_eq!(run("Number('\\uFEFF42')"), "42");
    assert_eq!(run("parseInt('\\uFEFF10')"), "10");
}

#[test]
fn string_replace_substitution() {
    assert_eq!(run("'abc'.replace('b', '[$`]')"), "a[a]c");
    assert_eq!(run("'abc'.replace('b', \"[$']\")"), "a[c]c");
    assert_eq!(run("'aaa'.replaceAll('a', '$&$&')"), "aaaaaa");
    // An empty search inserts between every character.
    assert_eq!(run("'ab'.replaceAll('', '-')"), "-a-b-");
}

#[test]
fn json_stringify_replacer() {
    // Array replacer restricts (and orders) the keys.
    assert_eq!(
        run("JSON.stringify({a:1,b:2,c:3}, ['c','a'])"),
        r#"{"c":3,"a":1}"#
    );
    assert_eq!(run("JSON.stringify({a:1,b:2}, [])"), "{}");
    // Function replacer transforms values.
    assert_eq!(
        run("JSON.stringify({a:1,b:2}, (k,v)=>typeof v==='number'?v*10:v)"),
        r#"{"a":10,"b":20}"#
    );
}

#[test]
fn error_is_error_and_stack() {
    assert_eq!(run("Error.isError(new TypeError())"), "true");
    assert_eq!(run("Error.isError({})"), "false");
    assert_eq!(run("Error.isError(null)"), "false");
    // stack is an accessor; the setter shadows it with an own data property.
    assert_eq!(
        run("typeof Object.getOwnPropertyDescriptor(Error.prototype,'stack').get"),
        "function"
    );
    assert_eq!(run("var e=new Error(); e.stack='x'; e.stack"), "x");
}

#[test]
fn bound_function_length_name() {
    assert_eq!(run("function f(a,b,c){} f.bind(null).length"), "3");
    assert_eq!(run("function f(a,b,c){} f.bind(null, 1).length"), "2");
    assert_eq!(run("function f(a,b){} f.bind(null,1,2,3).length"), "0");
    assert_eq!(run("function foo(){} foo.bind(null).name"), "bound foo");
    assert_eq!(
        run("function foo(){} foo.bind(null).bind(null).name"),
        "bound bound foo"
    );
}

#[test]
fn new_target_basics() {
    // A constructor's new.target is the constructor; a plain call's is undefined.
    assert_eq!(
        run("var t; function F(){ t = new.target; } new F(); t === F"),
        "true"
    );
    assert_eq!(
        run("var t='x'; function F(){ t = new.target; } F(); t"),
        "undefined"
    );
    // Reflect.construct honors its newTarget argument's prototype.
    assert_eq!(
        run("function A(){} function B(){} var o=Reflect.construct(A,[],B); Object.getPrototypeOf(o)===B.prototype"),
        "true"
    );
}

#[test]
fn weak_collections_symbol_keys() {
    assert_eq!(
        run("var s=Symbol(); var m=new WeakMap(); m.set(s,1); m.get(s)"),
        "1"
    );
    assert_eq!(
        run("var s=Symbol(); var w=new WeakSet(); w.add(s); w.has(s)"),
        "true"
    );
    // A registered symbol is not collectable, so it can't be a weak key.
    assert_eq!(throws("new WeakMap().set(Symbol.for('x'), 1)"), "TypeError");
}

#[test]
fn iterator_helpers_close_and_from() {
    // Eager helpers close the underlying iterator when the callback throws.
    assert_eq!(
        run(r#"
            var closed = false;
            var iter = { next(){ return {done:false, value:1}; }, return(){ closed=true; return {}; } };
            try { Iterator.from(iter).forEach(()=>{ throw 0; }); } catch(e) {}
            closed
        "#),
        "true"
    );
    // A non-callable predicate is a TypeError that still closes the source.
    assert_eq!(
        run(r#"
            var closed=false;
            var iter={ next(){return{done:false,value:1};}, return(){closed=true;return{};} };
            try { Iterator.from(iter).every(5); } catch(e) {}
            closed
        "#),
        "true"
    );
    // Iterator.from accepts a bare iterator (no @@iterator) and exposes the helpers.
    assert_eq!(
        run(r#"
            var i=0;
            var bare={ next(){ return i<3?{done:false,value:++i}:{done:true}; } };
            Iterator.from(bare).map(x=>x*2).toArray().join(',')
        "#),
        "2,4,6"
    );
    // Iterator.from on a string iterates its characters.
    assert_eq!(run("Iterator.from('abc').toArray().join('-')"), "a-b-c");
    // flatMap rejects a primitive mapper result.
    assert_eq!(throws("[1].values().flatMap(x=>x).toArray()"), "TypeError");
    // flatMap flattens an iterator result.
    assert_eq!(
        run("[1,2].values().flatMap(x=>[x,x].values()).toArray().join(',')"),
        "1,1,2,2"
    );
    // take validates its limit (RangeError) and closes once on return().
    assert_eq!(throws("[1,2].values().take(-1)"), "RangeError");
    assert_eq!(throws("[1,2].values().take(NaN)"), "RangeError");
}

#[test]
fn iterator_take_drop() {
    assert_eq!(
        run("[1,2,3,4,5].values().take(2).toArray().join(',')"),
        "1,2"
    );
    assert_eq!(
        run("[1,2,3,4,5].values().drop(2).toArray().join(',')"),
        "3,4,5"
    );
    assert_eq!(
        run("[1,2,3].values().take(10).toArray().join(',')"),
        "1,2,3"
    );
}

#[test]
fn iterator_zip_basics() {
    assert_eq!(
        run("Iterator.zip([[1,2],[3,4]]).map(p=>p.join('')).toArray().join(',')"),
        "13,24"
    );
    // shortest mode (default) stops at the shortest input.
    assert_eq!(run("Iterator.zip([[1,2,3],[4,5]]).toArray().length"), "2");
    // longest mode pads the missing values.
    assert_eq!(
        run("Iterator.zip([[1],[2,3]], {mode:'longest'}).toArray().map(p=>p.join('|')).join(',')"),
        "1|2,|3"
    );
    // zipKeyed pairs object keys.
    assert_eq!(
        run("var z=Iterator.zipKeyed({a:[1,2],b:[3,4]}).toArray(); z[0].a+''+z[0].b"),
        "13"
    );
    // An invalid mode is a RangeError.
    assert_eq!(throws("Iterator.zip([[1]], {mode:'bogus'})"), "RangeError");
}

#[test]
fn iterator_helper_return_propagates() {
    // A helper's return() propagates an error thrown by the source's return method.
    assert_eq!(
        run(r#"
            var src={ next(){return{done:false,value:1};}, return(){ throw new TypeError('x'); } };
            var h=Iterator.from(src).map(x=>x);
            h.next();
            var caught='no';
            try { h.return(); } catch(e) { caught=e.constructor.name; }
            caught
        "#),
        "TypeError"
    );
}

#[test]
fn iterator_take_exhaustion_closes() {
    // take(0) closes the source immediately, propagating its return() error.
    assert_eq!(
        run(r#"
            var src={ next(){return{done:false,value:1};}, return(){ throw new RangeError('r'); } };
            var caught='no';
            try { Iterator.from(src).take(0).next(); } catch(e){ caught=e.constructor.name; }
            caught
        "#),
        "RangeError"
    );
    // A normal take stops at the limit.
    assert_eq!(run("[1,2,3].values().take(2).toArray().length"), "2");
}

#[test]
fn iterator_eager_close_on_found_propagates() {
    // some/find close the source when a match is found, propagating its return() error.
    assert_eq!(
        run(r#"
            var src={ i:0, next(){ return {done:false, value:++this.i}; }, return(){ throw new RangeError(); } };
            var caught='no';
            try { Iterator.from(src).some(x=>x===2); } catch(e){ caught=e.constructor.name; }
            caught
        "#),
        "RangeError"
    );
    assert_eq!(run("[1,2,3,4].values().some(x=>x===3)"), "true");
    assert_eq!(run("[1,2,3,4].values().find(x=>x>2)"), "3");
}

#[test]
fn iterator_zip_modes() {
    // strict mode throws on a length mismatch.
    assert_eq!(
        throws("Iterator.zip([[1,2],[3]], {mode:'strict'}).toArray()"),
        "TypeError"
    );
    // equal-length strict succeeds.
    assert_eq!(
        run("Iterator.zip([[1,2],[3,4]], {mode:'strict'}).toArray().length"),
        "2"
    );
    // shortest closes the longer iterator when the shorter finishes.
    assert_eq!(
        run(r#"
            var closed=false;
            var long={ i:0, next(){ return {done:false, value:++this.i}; }, return(){ closed=true; return {}; } };
            Iterator.zip([[1], long]).toArray();
            closed
        "#),
        "true"
    );
}

#[test]
fn boxed_symbol_wrapper() {
    // Object(symbol) yields a Symbol wrapper object whose prototype methods unwrap it.
    assert_eq!(run("typeof Object(Symbol('z'))"), "object");
    assert_eq!(
        run("Symbol.prototype.toString.call(Object(Symbol('z')))"),
        "Symbol(z)"
    );
    assert_eq!(
        run("var s=Symbol('q'); Symbol.prototype.valueOf.call(Object(s))===s"),
        "true"
    );
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(Symbol.prototype,'description').get.call(Object(Symbol('d')))"),
        "d"
    );
}

#[test]
fn boxed_bigint_wrapper() {
    // Object(bigint) yields a BigInt wrapper object whose prototype methods unwrap it.
    assert_eq!(run("typeof Object(10n)"), "object");
    assert_eq!(
        run("BigInt.prototype.toString.call(Object(255n), 16)"),
        "ff"
    );
    assert_eq!(
        run("BigInt.prototype.valueOf.call(Object(42n)) === 42n"),
        "true"
    );
}

#[test]
fn iterator_concat_return_closes_inner() {
    // The concat result iterator's return() closes the currently-open inner iterator.
    assert_eq!(
        run(r#"
            var closed=false;
            var inner={ next(){ return {done:false, value:1}; }, return(){ closed=true; return {}; }, [Symbol.iterator](){ return this; } };
            var it=Iterator.concat(inner);
            it.next();
            it.return();
            closed
        "#),
        "true"
    );
    // After return(), subsequent next() reports done without re-opening.
    assert_eq!(
        run(r#"
            var it=Iterator.concat([1,2,3]);
            it.next(); it.return();
            it.next().done
        "#),
        "true"
    );
}

#[test]
fn symbol_proto_to_primitive_and_tag() {
    // Symbol.prototype[@@toPrimitive] unwraps a Symbol wrapper.
    assert_eq!(
        run("Object(Symbol.toPrimitive)[Symbol.toPrimitive]() === Symbol.toPrimitive"),
        "true"
    );
    // @@toStringTag is "Symbol" and drives Object.prototype.toString.
    assert_eq!(run("Symbol.prototype[Symbol.toStringTag]"), "Symbol");
    assert_eq!(
        run("Object.prototype.toString.call(Object(Symbol()))"),
        "[object Symbol]"
    );
    // The @@toPrimitive property is non-writable, non-enumerable, configurable.
    assert_eq!(
        run("var d=Object.getOwnPropertyDescriptor(Symbol.prototype, Symbol.toPrimitive); [d.writable,d.enumerable,d.configurable].join(',')"),
        "false,false,true"
    );
}

#[test]
fn bigint_constructor_string_radix() {
    // Radix prefixes, sign, empty, and whitespace trimming in BigInt(string).
    assert_eq!(run("BigInt('0x10') === 16n"), "true");
    assert_eq!(run("BigInt('0o17') === 15n"), "true");
    assert_eq!(run("BigInt('0b101') === 5n"), "true");
    assert_eq!(run("BigInt('  -42  ') === -42n"), "true");
    assert_eq!(run("BigInt('') === 0n"), "true");
    assert_eq!(throws("BigInt('0x')"), "SyntaxError");
    assert_eq!(throws("BigInt('1.5')"), "SyntaxError");
    // BigInt(object) coerces via ToPrimitive(number) then ToBigInt.
    assert_eq!(run("BigInt({valueOf(){return 7n;}}) === 7n"), "true");
}

#[test]
fn bigint_asintn_uintn_coercion() {
    // bits via ToIndex, value via ToBigInt (booleans, strings, objects accepted).
    assert_eq!(run("BigInt.asUintN(8, 258n)"), "2");
    assert_eq!(run("BigInt.asIntN(8, 255n)"), "-1");
    assert_eq!(run("BigInt.asUintN(4, true)"), "1");
    assert_eq!(run("BigInt.asUintN('8', '258')"), "2");
    // @@toStringTag drives Object.prototype.toString for BigInt wrappers.
    assert_eq!(run("BigInt.prototype[Symbol.toStringTag]"), "BigInt");
    assert_eq!(
        run("Object.prototype.toString.call(Object(1n))"),
        "[object BigInt]"
    );
}

#[test]
fn json_stringify_proxy_and_wrappers() {
    // Proxies serialize via their ownKeys/get traps (and IsArray sees through them).
    assert_eq!(run("JSON.stringify(new Proxy({a:1}, {}))"), r#"{"a":1}"#);
    assert_eq!(run("JSON.stringify(new Proxy([1,2], {}))"), "[1,2]");
    // Primitive wrappers unwrap to their primitive.
    assert_eq!(
        run("JSON.stringify({n:Object(5), s:Object('x'), b:Object(true)})"),
        r#"{"n":5,"s":"x","b":true}"#
    );
    // A BigInt wrapper (or primitive) still throws when serialized without toJSON.
    assert_eq!(throws("JSON.stringify(Object(1n))"), "TypeError");
    assert_eq!(throws("JSON.stringify(1n)"), "TypeError");
}

#[test]
fn json_stringify_space_and_replacer_tostring() {
    // A Number-wrapper space arg is unwrapped via ToNumber.
    assert_eq!(
        run("JSON.stringify({a:1}, null, Object(2))"),
        "{\n  \"a\": 1\n}"
    );
    // A replacer-array entry that is a String wrapper contributes ToString(entry) as the key.
    assert_eq!(
        run(r#"
            var s=new String('x'); s.toString=function(){return 'k';};
            JSON.stringify({k:1, x:2}, [s])
        "#),
        r#"{"k":1}"#
    );
    // BigInt with a toJSON serializes the toJSON result instead of throwing.
    assert_eq!(
        run("BigInt.prototype.toJSON=function(){return 'big';}; var r=JSON.stringify(5n); delete BigInt.prototype.toJSON; r"),
        r#""big""#
    );
}

#[test]
fn json_parse_reviver() {
    // The reviver transforms values bottom-up; returning undefined deletes the key.
    assert_eq!(
        run("JSON.parse('{\"a\":1,\"b\":2}', (k,v)=> typeof v==='number'? v*10 : v).a"),
        "10"
    );
    assert_eq!(
        run("var o=JSON.parse('{\"x\":1,\"y\":2}', (k,v)=> k==='y'? undefined : v); 'y' in o"),
        "false"
    );
    // The reviver is called with keys bottom-up then the root "".
    assert_eq!(
        run("var ks=[]; JSON.parse('{\"a\":[1,2]}', function(k,v){ks.push(k);return v;}); ks.join(',')"),
        "0,1,a,"
    );
}

#[test]
fn json_parse_reviver_context_source() {
    // A primitive leaf exposes its exact source text via the context's `source` property.
    assert_eq!(run("JSON.parse('1.50', (k,v,ctx)=> ctx.source)"), "1.50");
    // A forward-modified element reports no source (the value is no longer the parsed one).
    assert_eq!(
        run(r#"
            (function(){
                var seen = 'unset';
                JSON.parse('[1,2]', function(k,v,ctx){
                    if (k==='0') this[1] = 99;
                    if (k==='1') seen = ctx.source;
                    return this[k];
                });
                return String(seen);
            })()
        "#),
        "undefined"
    );
    // CreateDataProperty during revival respects a non-configurable existing property (no throw).
    assert_eq!(
        run(r#"
            var o=JSON.parse('{"a":1,"b":2}', function(k,v){
                if (k==='a') Object.defineProperty(this,'b',{configurable:false});
                return k==='b'? 42 : v;
            });
            o.b
        "#),
        "2"
    );
}

#[test]
fn object_assign_semantics() {
    // ToObject(target) throws for null/undefined.
    assert_eq!(throws("Object.assign(null, {})"), "TypeError");
    // Symbol-keyed and string-keyed enumerable own properties are copied; result is the target.
    assert_eq!(
        run("var s=Symbol(); var t={}; var r=Object.assign(t, {a:1}, (function(){var o={};o[s]=2;return o;})()); [r===t, r.a, r[s]].join(',')"),
        "true,1,2"
    );
    // Assigning to a non-writable target property throws.
    assert_eq!(
        throws("var t=Object.defineProperty({}, 'x', {value:1, writable:false}); Object.assign(t, {x:2})"),
        "TypeError"
    );
    // null/undefined sources are skipped.
    assert_eq!(
        run("Object.keys(Object.assign({}, null, undefined, {a:1})).join(',')"),
        "a"
    );
    // A Proxy source is read through its ownKeys/get traps.
    assert_eq!(run("Object.assign({}, new Proxy({a:5}, {})).a"), "5");
}

#[test]
fn object_descriptors_coercion() {
    // getOwnPropertyDescriptors / getOwnPropertySymbols coerce primitives via ToObject.
    assert_eq!(run("Object.getOwnPropertyDescriptors('ab')[0].value"), "a");
    assert_eq!(run("Object.getOwnPropertySymbols('x').length"), "0");
    assert_eq!(
        throws("Object.getOwnPropertyDescriptors(null)"),
        "TypeError"
    );
    assert_eq!(
        throws("Object.getOwnPropertySymbols(undefined)"),
        "TypeError"
    );
}

#[test]
fn object_from_entries() {
    assert_eq!(run("Object.fromEntries([['a',1],['b',2]]).b"), "2");
    // null/undefined input throws; a non-object entry throws.
    assert_eq!(throws("Object.fromEntries(null)"), "TypeError");
    assert_eq!(throws("Object.fromEntries([1,2])"), "TypeError");
    // Uses CreateDataProperty: an inherited setter on the key is not triggered.
    assert_eq!(
        run(r#"
            var triggered=false;
            Object.defineProperty(Object.prototype, 'p', {configurable:true, set(){triggered=true;}});
            var o=Object.fromEntries([['p', 1]]);
            delete Object.prototype.p;
            [o.p, triggered].join(',')
        "#),
        "1,false"
    );
}

#[test]
fn collection_brand_checks() {
    // A prototype method rejects a receiver of a different collection brand.
    assert_eq!(
        throws("Map.prototype.set.call(new Set(), 1, 2)"),
        "TypeError"
    );
    assert_eq!(throws("Set.prototype.add.call(new Map(), 1)"), "TypeError");
    assert_eq!(
        throws("WeakMap.prototype.set.call(new Map(), {}, 1)"),
        "TypeError"
    );
    assert_eq!(
        throws("Map.prototype.get.call(new WeakMap(), {})"),
        "TypeError"
    );
    assert_eq!(throws("WeakMap.prototype.get.call({}, {})"), "TypeError");
    // Same-brand calls still work.
    assert_eq!(run("var m=new Map(); m.set(1,2); m.get(1)"), "2");
    assert_eq!(
        run("var s=new Set([1,2,3]); s.union(new Set([3,4])).size"),
        "4"
    );
}

#[test]
fn weakmap_get_or_insert() {
    // getOrInsert returns the existing value, or inserts and returns the supplied value.
    assert_eq!(
        run("var k={}; var w=new WeakMap(); [w.getOrInsert(k, 1), w.getOrInsert(k, 2)].join(',')"),
        "1,1"
    );
    // getOrInsertComputed calls the callback only when the key is absent.
    assert_eq!(
        run("var k={}; var w=new WeakMap([[k, 9]]); w.getOrInsertComputed(k, ()=>{throw 'no';})"),
        "9"
    );
    // A non-registerable key throws.
    assert_eq!(throws("new WeakMap().getOrInsert(5, 1)"), "TypeError");
}

#[test]
fn set_operations_spec() {
    assert_eq!(
        run("[...new Set([1,2,3]).union(new Set([3,4]))].join(',')"),
        "1,2,3,4"
    );
    assert_eq!(
        run("[...new Set([1,2,3]).intersection(new Set([2,3,4]))].join(',')"),
        "2,3"
    );
    assert_eq!(
        run("[...new Set([1,2,3]).difference(new Set([2]))].join(',')"),
        "1,3"
    );
    assert_eq!(
        run("[...new Set([1,2]).symmetricDifference(new Set([2,3]))].join(',')"),
        "1,3"
    );
    assert_eq!(run("new Set([1,2]).isSubsetOf(new Set([1,2,3]))"), "true");
    assert_eq!(run("new Set([1,2,3]).isSubsetOf(new Set([1,2]))"), "false");
    assert_eq!(run("new Set([1,2]).isDisjointFrom(new Set([3,4]))"), "true");
    // A negative set-like size throws RangeError.
    assert_eq!(
        throws("new Set([1]).union({size:-1, has(){}, keys(){}})"),
        "RangeError"
    );
}

#[test]
fn number_constants_and_tofixed() {
    // The numeric constants are non-writable/enumerable/configurable.
    assert_eq!(
        run("var d=Object.getOwnPropertyDescriptor(Number,'MAX_VALUE'); [d.writable,d.enumerable,d.configurable].join(',')"),
        "false,false,false"
    );
    assert_eq!(run("Number.MAX_VALUE = 1; Number.MAX_VALUE === 1"), "false");
    // toFixed() defaults its argument to 0 (ToIntegerOrInfinity of undefined).
    assert_eq!(run("(3.14159).toFixed()"), "3");
    assert_eq!(run("(3.14159).toFixed(2)"), "3.14");
    // Out-of-range still throws RangeError.
    assert_eq!(throws("(1).toFixed(101)"), "RangeError");
}

#[test]
fn date_setter_order_and_invalid() {
    // thisTimeValue validation precedes argument coercion: a non-Date receiver throws
    // before the argument's valueOf runs.
    assert_eq!(
        run(r#"
            var called=false;
            try { Date.prototype.setHours.call({}, {valueOf(){called=true;return 0;}}); } catch(e){}
            called
        "#),
        "false"
    );
    // An invalid (NaN) date: the setter returns NaN and leaves [[DateValue]] untouched, so a
    // valueOf side-effect on the receiver persists.
    assert_eq!(
        run(r#"
            var dt=new Date(NaN);
            var r=dt.setHours({valueOf(){ dt.setTime(0); return 1; }});
            [Number.isNaN(r), dt.getTime()].join(',')
        "#),
        "true,0"
    );
}

#[test]
fn math_constants_and_hypot() {
    // All Math constants exist and are non-writable/enumerable/configurable.
    assert_eq!(
        run("typeof Math.LOG2E + ',' + typeof Math.LOG10E + ',' + typeof Math.SQRT1_2"),
        "number,number,number"
    );
    assert_eq!(
        run("var d=Object.getOwnPropertyDescriptor(Math,'PI'); [d.writable,d.enumerable,d.configurable].join(',')"),
        "false,false,false"
    );
    assert_eq!(run("Math.PI = 3; Math.PI === 3"), "false");
    assert_eq!(run("Math[Symbol.toStringTag]"), "Math");
    // hypot: an infinite operand wins over NaN.
    assert_eq!(run("Math.hypot(Infinity, NaN)"), "Infinity");
    assert_eq!(run("Math.hypot(3, 4)"), "5");
    assert_eq!(run("Number.isNaN(Math.hypot(NaN, 2))"), "true");
}

#[test]
fn global_value_property_descriptors() {
    for name in ["undefined", "NaN", "Infinity"] {
        let src = format!(
            "var d=Object.getOwnPropertyDescriptor(globalThis,'{name}'); [d.writable,d.enumerable,d.configurable].join(',')"
        );
        assert_eq!(run(&src), "false,false,false", "descriptor for {name}");
    }
    assert_eq!(run("typeof undefined"), "undefined");
    assert_eq!(run("Number.isNaN(NaN)"), "true");
}

#[test]
fn math_sum_precise() {
    assert_eq!(run("Math.sumPrecise([1,2,3])"), "6");
    // Exactly rounded despite catastrophic cancellation.
    assert_eq!(run("Math.sumPrecise([1, 1e100, 1, -1e100])"), "2");
    // Empty input is -0; mixed infinities are NaN.
    assert_eq!(run("1/Math.sumPrecise([])"), "-Infinity");
    assert_eq!(
        run("Number.isNaN(Math.sumPrecise([Infinity, -Infinity]))"),
        "true"
    );
    assert_eq!(run("Math.sumPrecise([Infinity, 5])"), "Infinity");
    // A non-number element throws.
    assert_eq!(throws("Math.sumPrecise([1, '2'])"), "TypeError");
}

#[test]
fn array_to_locale_string() {
    assert_eq!(run("[1,2,3].toLocaleString()"), "1,2,3");
    // null/undefined elements contribute empty strings.
    assert_eq!(run("[1,null,undefined,2].toLocaleString()"), "1,,,2");
    // Each element's own toLocaleString is invoked.
    assert_eq!(
        run("[{toLocaleString(){return 'X';}}, {toLocaleString(){return 'Y';}}].toLocaleString()"),
        "X,Y"
    );
}

#[test]
fn array_sort_holes_and_delete() {
    // Holes sort to the very end and remain holes (not own undefined properties).
    assert_eq!(
        run("var a=[3,,1,undefined]; a.sort(); [a.join(','), a.length, a.hasOwnProperty(3)].join('|')"),
        "1,3,,|4|false"
    );
    // Present undefined sorts after defined values but before holes.
    assert_eq!(
        run("var a=[3,undefined,1]; a.sort((x,y)=>x-y); a.join(',')"),
        "1,3,"
    );
    // A non-callable, non-undefined comparator throws.
    assert_eq!(throws("[1,2].sort({})"), "TypeError");
}

#[test]
fn array_flat_flatmap_holes() {
    // flatMap validates the callback and skips holes; flat skips holes too.
    assert_eq!(
        run("[1,2,3].flatMap(x=>[x,x*10]).join(',')"),
        "1,10,2,20,3,30"
    );
    assert_eq!(throws("[1].flatMap(5)"), "TypeError");
    assert_eq!(run("var c=0; [1,,3].flatMap(x=>{c++;return x;}); c"), "2");
    assert_eq!(run("[1,[2,[3]]].flat().join(',')"), "1,2,3");
    assert_eq!(run("[1,[2,[3]]].flat(2).join(',')"), "1,2,3");
}

#[test]
fn array_reduce_right_holes_and_callable() {
    assert_eq!(run("[1,2,3].reduceRight((a,b)=>a+'-'+b)"), "3-2-1");
    // Holes are skipped.
    assert_eq!(
        run("var c=0; [1,,3].reduceRight((a,b)=>{c++;return a;}, 0); c"),
        "2"
    );
    // A non-callable callback throws TypeError.
    assert_eq!(throws("[1,2].reduceRight(5)"), "TypeError");
    // Empty array with no initial value throws.
    assert_eq!(throws("[].reduceRight((a,b)=>a)"), "TypeError");
}

#[test]
fn array_of_constructor() {
    assert_eq!(run("Array.of(1,2,3).join(',')"), "1,2,3");
    assert_eq!(run("Array.isArray(Array.of(7))"), "true");
    // Honors a custom `this` constructor.
    assert_eq!(
        run("function C(n){this.n=n;} var r=Array.of.call(C,'a','b'); [r instanceof C, r[0], r.length].join(',')"),
        "true,a,2"
    );
}

#[test]
fn array_copy_within_holes() {
    assert_eq!(run("[1,2,3,4,5].copyWithin(0,3).join(',')"), "4,5,3,4,5");
    // Copying from a hole deletes the destination index.
    assert_eq!(
        run("var a=[1,2,3]; delete a[1]; a.copyWithin(0,1); [a.hasOwnProperty(0), a[1]].join(',')"),
        "false,3"
    );
}

#[test]
fn array_concat_spreadable_and_proxy() {
    assert_eq!(run("[1,2].concat([3,4],5).join(',')"), "1,2,3,4,5");
    // IsArray sees through a proxy, so a proxied array is spread.
    assert_eq!(run("[1].concat(new Proxy([2,3],{})).length"), "3");
    // @@isConcatSpreadable forces (or suppresses) spreading.
    assert_eq!(
        run("var o={length:2,0:'a',1:'b'}; o[Symbol.isConcatSpreadable]=true; [].concat(o).join(',')"),
        "a,b"
    );
    assert_eq!(
        run("var a=[1,2]; a[Symbol.isConcatSpreadable]=false; [].concat(a).length"),
        "1"
    );
}

#[test]
fn array_reverse_holes() {
    assert_eq!(run("[1,2,3].reverse().join(',')"), "3,2,1");
    // A hole reverses as a hole (moved by delete), not as own undefined.
    assert_eq!(
        run("var a=[1,,3]; a.reverse(); [a[0], a.hasOwnProperty(1), a[2]].join(',')"),
        "3,false,1"
    );
}

#[test]
fn array_splice_holes_and_shift() {
    assert_eq!(
        run("var a=[1,2,3,4,5]; var r=a.splice(1,2,'x'); a.join(',')+'|'+r.join(',')"),
        "1,x,4,5|2,3"
    );
    // Growing shifts the tail right correctly.
    assert_eq!(
        run("var c=[1,2,3]; c.splice(1,0,'a','b'); c.join(',')"),
        "1,a,b,2,3"
    );
    // Removed array preserves holes.
    assert_eq!(
        run("var b=[1,,3,4]; var r=b.splice(0,2); [r.hasOwnProperty(1), b.join(',')].join('|')"),
        "false|3,4"
    );
}

#[test]
fn date_to_json_generic() {
    // toJSON is generic: it invokes the receiver's toISOString after a finite ToPrimitive(number).
    assert_eq!(
        run("Date.prototype.toJSON.call({toISOString(){return 'ISO';}, valueOf(){return 1;}})"),
        "ISO"
    );
    // A non-finite time value yields null without invoking toISOString.
    assert_eq!(
        run("Date.prototype.toJSON.call({valueOf(){return NaN;}, toISOString(){return 'x';}})"),
        "null"
    );
    assert_eq!(run("typeof new Date(0).toJSON()"), "string");
}

#[test]
fn regexp_flags_getter_generic() {
    assert_eq!(run("/abc/gi.flags"), "gi");
    assert_eq!(run("/x/dgimsy.flags"), "dgimsy");
    // The flags getter is generic — it reads each component accessor from the receiver.
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(RegExp.prototype,'flags').get.call({global:true, sticky:true, hasIndices:true})"),
        "dgy"
    );
    // RegExp.prototype itself yields empty flags.
    assert_eq!(run("RegExp.prototype.flags"), "");
}

#[test]
fn string_matchall_replaceall_regexp_rules() {
    // matchAll/replaceAll throw for a non-global RegExp argument.
    assert_eq!(throws("'abc'.matchAll(/a/)"), "TypeError");
    assert_eq!(throws("'abc'.replaceAll(/a/, 'x')"), "TypeError");
    // A global RegExp works.
    assert_eq!(run("[...'aba'.matchAll(/a/g)].length"), "2");
    assert_eq!(run("'aba'.replaceAll(/a/g, 'x')"), "xbx");
    // replaceAll delegates to a custom @@replace on the search value.
    assert_eq!(
        run("var o={ [Symbol.replace](s,r){ return 'CUSTOM'; } }; 'hello'.replaceAll(o, 'x')"),
        "CUSTOM"
    );
    // String search with $$ / $& substitution.
    assert_eq!(run("'aaa'.replaceAll('a', '$$')"), "$$$");
    assert_eq!(run("'aaa'.replaceAll('a', '[$&]')"), "[a][a][a]");
}

#[test]
fn reflect_set_receiver() {
    // With a distinct receiver, the assignment lands on the receiver, not the target.
    assert_eq!(
        run("var t={}, r={}; Reflect.set(t,'x',5,r); [t.hasOwnProperty('x'), r.x].join(',')"),
        "false,5"
    );
    // A non-writable data property on the target makes the set fail (returns false).
    assert_eq!(
        run(
            "var t=Object.defineProperty({}, 'x', {value:1, writable:false}); Reflect.set(t,'x',2)"
        ),
        "false"
    );
    // An inherited setter is invoked with the receiver as `this`.
    assert_eq!(
        run("var got; var proto={set p(v){got=this;}}; var r=Object.create(proto); Reflect.set(r,'p',1,r); got===r"),
        "true"
    );
}

#[test]
fn arraybuffer_accessor_getters() {
    // byteLength/maxByteLength/resizable are accessor getters on the prototype, not own props.
    assert_eq!(run("new ArrayBuffer(8).byteLength"), "8");
    assert_eq!(
        run("new ArrayBuffer(8).hasOwnProperty('byteLength')"),
        "false"
    );
    assert_eq!(
        run("typeof Object.getOwnPropertyDescriptor(ArrayBuffer.prototype,'byteLength').get"),
        "function"
    );
    // A resizable buffer reports its max and resizes.
    assert_eq!(
        run("var b=new ArrayBuffer(4, {maxByteLength:16}); [b.resizable, b.maxByteLength].join(',')"),
        "true,16"
    );
    assert_eq!(
        run("var b=new ArrayBuffer(4, {maxByteLength:16}); b.resize(10); b.byteLength"),
        "10"
    );
    assert_eq!(run("new ArrayBuffer(8).resizable"), "false");
    // A detached buffer reports 0 byteLength and detached=true.
    assert_eq!(
        run("var b=new ArrayBuffer(8); b.transfer(); [b.byteLength, b.detached].join(',')"),
        "0,true"
    );
}

#[test]
fn shared_array_buffer_getters() {
    assert_eq!(run("new SharedArrayBuffer(8).byteLength"), "8");
    assert_eq!(
        run("new SharedArrayBuffer(8).hasOwnProperty('byteLength')"),
        "false"
    );
    assert_eq!(run("new SharedArrayBuffer(8).growable"), "false");
    assert_eq!(
        run("var s=new SharedArrayBuffer(4,{maxByteLength:16}); [s.growable, s.maxByteLength].join(',')"),
        "true,16"
    );
    assert_eq!(
        run("var s=new SharedArrayBuffer(4,{maxByteLength:16}); s.grow(12); s.byteLength"),
        "12"
    );
    assert_eq!(
        run("Object.prototype.toString.call(new SharedArrayBuffer(1))"),
        "[object SharedArrayBuffer]"
    );
}

#[test]
fn atomics_index_and_ops() {
    assert_eq!(
        run("var ta=new Int32Array(new SharedArrayBuffer(8)); Atomics.store(ta,0,42); Atomics.load(ta,0)"),
        "42"
    );
    // A fractional access index is truncated (ToIndex), not rejected.
    assert_eq!(
        run("var ta=new Int32Array(new SharedArrayBuffer(8)); Atomics.store(ta,1.9,7); Atomics.load(ta,1)"),
        "7"
    );
    assert_eq!(
        run("var ta=new Int32Array(new SharedArrayBuffer(8)); ta[0]=5; Atomics.add(ta,0,3); ta[0]"),
        "8"
    );
    // A non-integer TypedArray is rejected.
    assert_eq!(
        throws("Atomics.add(new Float64Array(2), 0, 1)"),
        "TypeError"
    );
    // Out-of-bounds index is a RangeError.
    assert_eq!(
        throws("Atomics.load(new Int32Array(new SharedArrayBuffer(8)), 5)"),
        "RangeError"
    );
}

#[test]
fn promise_resolve_reject_this() {
    // Promise.resolve returns an existing promise whose constructor is the receiver.
    assert_eq!(
        run("var p=Promise.resolve(1); Promise.resolve(p)===p"),
        "true"
    );
    // A non-object receiver throws TypeError.
    assert_eq!(throws("Promise.resolve.call(undefined, 1)"), "TypeError");
    assert_eq!(throws("Promise.reject.call(null, 1)"), "TypeError");
    // Resolve/reject still produce promises.
    assert_eq!(run("Promise.resolve(1) instanceof Promise"), "true");
    assert_eq!(
        run("Promise.reject(1).catch(()=>{}) instanceof Promise"),
        "true"
    );
}

#[test]
fn finalization_registry_validation() {
    assert_eq!(
        run("var f=new FinalizationRegistry(()=>{}); f.register({},'h'); true"),
        "true"
    );
    // Non-registerable target, target===held, bad token, and brand mismatch all throw.
    assert_eq!(
        throws("new FinalizationRegistry(()=>{}).register(5,'h')"),
        "TypeError"
    );
    assert_eq!(
        throws("var t={}; new FinalizationRegistry(()=>{}).register(t,t)"),
        "TypeError"
    );
    assert_eq!(
        throws("new FinalizationRegistry(()=>{}).register({},'h',5)"),
        "TypeError"
    );
    assert_eq!(
        throws("FinalizationRegistry.prototype.register.call({}, {}, 'h')"),
        "TypeError"
    );
    assert_eq!(
        run("Object.prototype.toString.call(new FinalizationRegistry(()=>{}))"),
        "[object FinalizationRegistry]"
    );
}

#[test]
fn weakref_brand_and_tag() {
    assert_eq!(run("var o={}; new WeakRef(o).deref()===o"), "true");
    assert_eq!(throws("WeakRef.prototype.deref.call({})"), "TypeError");
    assert_eq!(throws("new WeakRef(5)"), "TypeError");
    assert_eq!(
        run("Object.prototype.toString.call(new WeakRef({}))"),
        "[object WeakRef]"
    );
}

#[test]
fn promise_resolving_function_shape() {
    // The executor's resolve/reject functions have length 1 and an empty name.
    assert_eq!(
        run("var o; new Promise((res,rej)=>{o=[res.length,rej.length,res.name,rej.name];}); o.join('|')"),
        "1|1||"
    );
}

#[test]
fn reflect_completeness() {
    // apply/construct use CreateListFromArrayLike (array-like, not iteration).
    assert_eq!(
        run("Reflect.apply(Math.max, null, {length:2, 0:3, 1:9})"),
        "9"
    );
    assert_eq!(throws("Reflect.apply(Math.max, null, 5)"), "TypeError");
    // ownKeys order: integer indices ascending, then strings, then symbols.
    assert_eq!(
        run("var s=Symbol(); var o={}; o.b=1;o[2]=1;o.a=1;o[0]=1;o[1]=1;o[s]=1; var k=Reflect.ownKeys(o); k.slice(0,5).join(',')"),
        "0,1,2,b,a"
    );
    // get honors the receiver for accessors; setPrototypeOf detects cycles.
    assert_eq!(
        run("Reflect.get({get x(){return this.v;}}, 'x', {v:42})"),
        "42"
    );
    assert_eq!(
        run("var a={},b=Object.create(a); Reflect.setPrototypeOf(a,b)"),
        "false"
    );
    // has/getOwnPropertyDescriptor go through proxy traps.
    assert_eq!(
        run("var t=false; try{Reflect.has(new Proxy({},{has(){throw new TypeError();}}),'x');}catch(e){t=e instanceof TypeError;} t"),
        "true"
    );
    assert_eq!(
        run("Reflect.getOwnPropertyDescriptor(new Proxy({a:1},{}), 'a').value"),
        "1"
    );
}

#[test]
fn object_freeze_seal_integrity() {
    assert_eq!(
        run("var o=Object.freeze({a:1}); [Object.isFrozen(o), Object.isExtensible(o)].join(',')"),
        "true,false"
    );
    assert_eq!(
        run("var s=Object.seal({a:1}); [Object.isSealed(s), Object.isFrozen(s)].join(',')"),
        "true,false"
    );
    // freeze/seal invoke a proxy's traps (preventExtensions, ownKeys, defineProperty).
    assert_eq!(
        run(r#"
            var log=[];
            var p=new Proxy({a:1}, {
                preventExtensions(t){log.push('pe');Object.preventExtensions(t);return true;},
                ownKeys(t){log.push('ok');return Reflect.ownKeys(t);},
                defineProperty(t,k,d){log.push('dp');return Reflect.defineProperty(t,k,d);},
                getOwnPropertyDescriptor(t,k){return Reflect.getOwnPropertyDescriptor(t,k);}
            });
            Object.freeze(p);
            log.join(',')
        "#),
        "pe,ok,dp"
    );
}

#[test]
fn object_define_properties_spec() {
    // create/defineProperties handle symbol-keyed descriptors and ToObject(Properties).
    assert_eq!(
        run("var s=Symbol.for('s'); var o=Object.create(null,{x:{value:5,enumerable:true},[s]:{value:9}}); [o.x, o[s]].join(',')"),
        "5,9"
    );
    // A null Properties argument throws (ToObject(null)).
    assert_eq!(throws("Object.create({}, null)"), "TypeError");
    assert_eq!(throws("Object.defineProperties({}, null)"), "TypeError");
    // Only enumerable descriptor entries are applied.
    assert_eq!(
        run("Object.defineProperties({}, Object.defineProperty({}, 'skip', {value:{value:1}, enumerable:false})).hasOwnProperty('skip')"),
        "false"
    );
}

#[test]
fn get_prototype_of_and_error_subclassing() {
    // getPrototypeOf coerces all primitive types.
    assert_eq!(
        run("Object.getPrototypeOf(Symbol()) === Symbol.prototype"),
        "true"
    );
    assert_eq!(
        run("Object.getPrototypeOf(1n) === Object.getPrototypeOf(2n)"),
        "true"
    );
    assert_eq!(throws("Object.getPrototypeOf(null)"), "TypeError");
    // Native error subtypes have [[Prototype]] === Error.
    assert_eq!(run("Object.getPrototypeOf(TypeError) === Error"), "true");
    assert_eq!(run("Object.getPrototypeOf(RangeError) === Error"), "true");
    assert_eq!(
        run("Object.getPrototypeOf(AggregateError) === Error"),
        "true"
    );
    assert_eq!(
        run("Object.getPrototypeOf(Error) === Function.prototype"),
        "true"
    );
    assert_eq!(run("new TypeError() instanceof Error"), "true");
}

#[test]
fn atomics_methods_and_validation() {
    assert_eq!(
        run("typeof Atomics.waitAsync + ',' + typeof Atomics.pause"),
        "function,function"
    );
    // wait requires a shared buffer; a non-shared one throws.
    assert_eq!(
        run("var ta=new Int32Array(new SharedArrayBuffer(8)); Atomics.wait(ta,0,999)"),
        "not-equal"
    );
    assert_eq!(
        throws("Atomics.wait(new Int32Array(new ArrayBuffer(8)),0,0)"),
        "TypeError"
    );
    // Float (incl. Float16) typed arrays are rejected.
    assert_eq!(
        throws("Atomics.add(new Float64Array(new SharedArrayBuffer(8)),0,1)"),
        "TypeError"
    );
    // waitAsync returns a { async, value } record synchronously here.
    assert_eq!(
        run("var w=Atomics.waitAsync(new Int32Array(new SharedArrayBuffer(8)),0,999); [w.async,w.value].join(',')"),
        "false,not-equal"
    );
    // pause validates its optional integer argument.
    assert_eq!(run("Atomics.pause(); Atomics.pause(3); 'ok'"), "ok");
    assert_eq!(throws("Atomics.pause(1.5)"), "TypeError");
}

#[test]
fn shared_array_buffer_aliasing() {
    // Two TypedArrays over the same SharedArrayBuffer alias the same (registry-backed) memory.
    assert_eq!(
        run("var s=new SharedArrayBuffer(16); var a=new Int32Array(s); var b=new Int32Array(s); a[0]=42; b[0]"),
        "42"
    );
    assert_eq!(
        run("var s=new SharedArrayBuffer(16); var a=new Int32Array(s); var b=new Int32Array(s); Atomics.store(a,1,99); Atomics.load(b,1)"),
        "99"
    );
    // wait returns 'not-equal' immediately when the value already differs.
    assert_eq!(
        run("var a=new Int32Array(new SharedArrayBuffer(8)); a[0]=5; Atomics.wait(a,0,0)"),
        "not-equal"
    );
    // wait with timeout 0 times out immediately when the value matches.
    assert_eq!(
        run("var a=new Int32Array(new SharedArrayBuffer(8)); Atomics.wait(a,0,0,0)"),
        "timed-out"
    );
    // notify with no waiters returns 0.
    assert_eq!(
        run("Atomics.notify(new Int32Array(new SharedArrayBuffer(8)),0)"),
        "0"
    );
}

#[test]
fn atomics_wait_async() {
    // A value mismatch resolves synchronously (not async).
    assert_eq!(
        run("var a=new Int32Array(new SharedArrayBuffer(8)); a[0]=9; var r=Atomics.waitAsync(a,0,0); [r.async, r.value].join(',')"),
        "false,not-equal"
    );
    // A zero timeout times out synchronously.
    assert_eq!(
        run("var a=new Int32Array(new SharedArrayBuffer(8)); var r=Atomics.waitAsync(a,0,0,0); [r.async, r.value].join(',')"),
        "false,timed-out"
    );
    // Otherwise it returns a pending promise that resolves once notified (driven by the event loop).
    assert_eq!(
        run("var a=new Int32Array(new SharedArrayBuffer(8)); var out='?'; var r=Atomics.waitAsync(a,0,0,2000); r.value.then(function(v){out=v;}); Atomics.notify(a,0,1); out"),
        "?"
    );
    assert_eq!(
        run(r#"
            var a=new Int32Array(new SharedArrayBuffer(8));
            var out='pending';
            var r=Atomics.waitAsync(a,0,0,2000);
            r.value.then(function(v){ out=v; });
            Atomics.notify(a,0,1);
            // The event loop resolves the promise after the script; capture via a second microtask.
            Promise.resolve().then(function(){});
            r.async
        "#),
        "true"
    );
}

#[test]
fn dataview_length_tracking_and_toprimitive() {
    // A length-tracking DataView over a resizable buffer follows the buffer's current length.
    assert_eq!(
        run("var b=new ArrayBuffer(8,{maxByteLength:16}); var dv=new DataView(b); var a=dv.byteLength; b.resize(16); a+','+dv.byteLength"),
        "8,16"
    );
    // A shrunk resizable buffer makes an out-of-bounds fixed-length view throw on access.
    assert_eq!(
        throws("var b=new ArrayBuffer(16,{maxByteLength:16}); var dv=new DataView(b,8,8); b.resize(4); dv.getInt8(0)"),
        "TypeError"
    );
    // @@toStringTag and getter names.
    assert_eq!(run("DataView.prototype[Symbol.toStringTag]"), "DataView");
    assert_eq!(
        run("Object.getOwnPropertyDescriptor(DataView.prototype,'byteLength').get.name"),
        "get byteLength"
    );
    // A present-but-non-callable @@toPrimitive is a TypeError (via ToIndex(byteOffset)).
    assert_eq!(
        throws("var dv=new DataView(new ArrayBuffer(8)); dv.getInt8({[Symbol.toPrimitive]:1})"),
        "TypeError"
    );
    // A detached buffer is still an ArrayBuffer: ToNumber(byteOffset) runs before the detach throw.
    assert_eq!(
        run("var n=0; var ab=new ArrayBuffer(8); var t=ab.transfer(); var o={valueOf(){n++;return 0;}}; try{new DataView(ab,o);}catch(e){} n"),
        "1"
    );
}

#[test]
fn immutable_array_buffer() {
    // transferToImmutable produces an immutable buffer and detaches the source.
    assert_eq!(
        run("var a=new ArrayBuffer(8); var i=a.transferToImmutable(); [i.immutable, a.detached, i.byteLength].join(',')"),
        "true,true,8"
    );
    // Writing to an immutable buffer via a DataView throws TypeError (before reading arguments).
    assert_eq!(
        throws("var i=(new ArrayBuffer(8)).transferToImmutable(); new DataView(i).setInt8(0,1)"),
        "TypeError"
    );
    // Reads still work.
    assert_eq!(
        run("var i=(new ArrayBuffer(8)).transferToImmutable(); new DataView(i).getInt8(0)"),
        "0"
    );
    // sliceToImmutable copies a range without detaching the source.
    assert_eq!(
        run("var a=new ArrayBuffer(8); new DataView(a).setInt8(2,7); var s=a.sliceToImmutable(2,4); [s.immutable,s.byteLength,a.detached,new DataView(s).getInt8(0)].join(',')"),
        "true,2,false,7"
    );
}

#[test]
fn float16_rounds_once() {
    // 2^-25 + ε must round up to the smallest f16 subnormal (2^-24), not double-round to zero.
    assert_eq!(
        run("var dv=new DataView(new ArrayBuffer(8)); dv.setFloat16(0, 2.980232238769532e-8); dv.getFloat16(0)"),
        "5.960464477539063e-8"
    );
    // Exactly 2^-25 ties to even → zero.
    assert_eq!(
        run("var dv=new DataView(new ArrayBuffer(8)); dv.setFloat16(0, 2.9802322387695312e-8); dv.getFloat16(0)"),
        "0"
    );
    assert_eq!(run("Math.f16round(1.337)"), "1.3369140625");
}

#[test]
fn typedarray_iteration_semantics() {
    // Reflect.set writes a TypedArray element (integer-indexed exotic [[Set]]), not a shadow prop.
    assert_eq!(
        run("var a=new Float64Array([1,2,3]); Reflect.set(a,1,9); a[1]"),
        "9"
    );
    // Callback methods observe live element writes during iteration.
    assert_eq!(
        run("var a=new Int32Array([5,6,7]); var seen=[]; a.forEach(function(v,idx){ if(idx===0)a[1]=42; seen.push(v);}); seen.join(',')"),
        "5,42,7"
    );
    // The length is captured once; shrinking mid-iteration surfaces undefined for OOB indices.
    assert_eq!(
        run("var b=new ArrayBuffer(16,{maxByteLength:16}); var a=new Int32Array(b); a.fill(1); var seen=[]; a.forEach(function(v,idx){ if(idx===1)b.resize(4); seen.push(v);}); seen.map(String).join(',')"),
        "1,1,undefined,undefined"
    );
    // includes reads OOB as undefined (found), indexOf uses strict equality on in-bounds only.
    assert_eq!(run("new Uint8Array([1,2,3]).includes(2)"), "true");
    assert_eq!(run("new Uint8Array([1,2,3]).indexOf(2)"), "1");
    assert_eq!(run("new Uint8Array([1,2,3,2]).lastIndexOf(2)"), "3");
}

#[test]
fn typedarray_set_semantics() {
    // Copy from another TypedArray, with overlap (same buffer) handled via a snapshot.
    assert_eq!(
        run("var a=new Int32Array([1,2,3,4]); a.set(a.subarray(0,3),1); a.join(',')"),
        "1,1,2,3"
    );
    // ToObject a primitive source (a String) reads its indexed chars.
    assert_eq!(
        run("var a=new Uint8Array(3); a.set('12'); a.join(',')"),
        "1,2,0"
    );
    // Mixing BigInt and Number content types is a TypeError.
    assert_eq!(
        throws("new BigInt64Array(2).set(new Int32Array(1))"),
        "TypeError"
    );
    // Uint8Clamped rounds half to even.
    assert_eq!(
        run("var a=new Uint8ClampedArray(3); a.set([0.5,1.5,2.5]); a.join(',')"),
        "0,2,2"
    );
    // A negative offset is a RangeError; an oversized source too.
    assert_eq!(throws("new Int8Array(4).set([1],-1)"), "RangeError");
    assert_eq!(throws("new Int8Array(2).set([1,2,3])"), "RangeError");
}

#[test]
fn typedarray_sort_semantics() {
    // Default comparator is numeric, not lexicographic.
    assert_eq!(
        run("new Int32Array([10,4,6,8]).sort().join(',')"),
        "4,6,8,10"
    );
    // NaN sorts last, -0 before +0.
    assert_eq!(
        run("var a=new Float64Array([NaN,1,-0]); a.sort(); 1/a[0]"),
        "-Infinity"
    );
    // toSorted/toReversed return a new same-type array without mutating the source.
    assert_eq!(
        run("var a=new Uint8Array([3,1,2]); var b=a.toSorted(); a.join(',')+'|'+b.join(',')"),
        "3,1,2|1,2,3"
    );
    assert_eq!(
        run("new Uint8Array([1,2,3]).toReversed().join(',')"),
        "3,2,1"
    );
    // Custom comparefn.
    assert_eq!(
        run("new Int32Array([1,2,3]).sort((a,b)=>b-a).join(',')"),
        "3,2,1"
    );
    // Sorting an immutable-backed array throws.
    assert_eq!(throws("var i=(new Int32Array([3,1,2])).buffer.transferToImmutable(); new Int32Array(i).sort()"), "TypeError");
}

#[test]
fn typedarray_slice_and_subclass_buffer() {
    // slice copies a range into a species-created array; out-of-range indices stay zero.
    assert_eq!(
        run("new Int32Array([1,2,3,4,5]).slice(1,3).join(',')"),
        "2,3"
    );
    assert_eq!(
        run("new Int32Array([1,2,3,4,5]).slice(-2).join(',')"),
        "4,5"
    );
    // A TypedArray subclass carries its buffer slot onto the derived `this`.
    assert_eq!(run("class MyF extends Float32Array {}; var a=new MyF(4); [typeof a.buffer, a.byteLength, a instanceof Float32Array].join(',')"), "object,16,true");
    // slice via a subclass source builds a subclass result with a real buffer.
    assert_eq!(run("class MyU extends Uint8Array {}; var s=new MyU([1,2,3]).slice(1); [typeof s.buffer, s.join(',')].join('|')"), "object|2,3");
}

#[test]
fn typedarray_subarray_semantics() {
    // subarray shares the buffer (a view, not a copy).
    assert_eq!(
        run("var a=new Int32Array([1,2,3,4]); var s=a.subarray(1,3); s[0]=9; a.join(',')+'|'+s.join(',')"),
        "1,9,3,4|9,3"
    );
    // NaN/false end coerce to 0; a negative end counts from the end.
    assert_eq!(run("new Int8Array([1,2,3,4]).subarray(0,NaN).length"), "0");
    assert_eq!(
        run("new Int8Array([1,2,3,4]).subarray(0,-1).join(',')"),
        "1,2,3"
    );
    // A length-tracking source with no end stays length-tracking.
    assert_eq!(
        run("var b=new ArrayBuffer(16,{maxByteLength:32}); var a=new Int32Array(b); var s=a.subarray(1); var before=s.length; b.resize(32); before+','+s.length"),
        "3,7"
    );
    // subarray over a detached buffer throws (constructing a view on detached memory).
    assert_eq!(
        throws("var a=new Int32Array(4); var t=a.buffer.transfer(); a.subarray(0);"),
        "TypeError"
    );
}

#[test]
fn typedarray_identity_and_names() {
    // @@iterator is the same function object as values; toString is Array.prototype.toString.
    assert_eq!(
        run("Int8Array.prototype[Symbol.iterator]===Int8Array.prototype.values"),
        "true"
    );
    assert_eq!(
        run("Int8Array.prototype.toString===Array.prototype.toString"),
        "true"
    );
    // Accessor getter names are prefixed with "get ".
    assert_eq!(run("Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Int8Array.prototype),'length').get.name"), "get length");
    // toLocaleString on an out-of-bounds view throws.
    assert_eq!(throws("var b=new ArrayBuffer(16,{maxByteLength:16}); var a=new Int32Array(b,0,4); b.resize(4); a.toLocaleString()"), "TypeError");
}

#[test]
fn array_iterator_exhaustion_and_ta_bounds() {
    // An exhausted iterator stays done even if the array grows afterwards.
    assert_eq!(
        run("var a=[1]; var it=a[Symbol.iterator](); it.next(); var d=it.next().done; a.push(2,3); [d, it.next().done].join(',')"),
        "true,true"
    );
    // A TypedArray iterator over a shrunk-out-of-bounds view throws TypeError.
    assert_eq!(
        throws("var b=new ArrayBuffer(16,{maxByteLength:16}); var a=new Int32Array(b,0,4); var it=a[Symbol.iterator](); it.next(); b.resize(4); it.next();"),
        "TypeError"
    );
}

#[test]
fn typedarray_exotic_internals() {
    // getOwnPropertyDescriptor: a non-canonical numeric key ("+1", "1.0") is an ordinary property.
    assert_eq!(
        run("var a=new Int8Array(3); Object.getOwnPropertyDescriptor(a,'+1')"),
        "undefined"
    );
    assert_eq!(run("var a=new Int8Array(3); Object.defineProperty(a,'1.0',{value:9,configurable:true}); a['1.0']"), "9");
    // A valid index write via a plain-object receiver whose proto is a TA creates on the receiver.
    assert_eq!(
        run("var t=new Int8Array([5]); var r=Object.create(t); r[0]=9; t[0]+','+r[0]"),
        "5,9"
    );
    // Reflect.set with a TypedArray receiver writes the element.
    assert_eq!(
        run("var t=new Int8Array([5]); var r=new Int8Array([7]); Reflect.set(t,0,3,r); r[0]"),
        "3"
    );
    // Strict-mode delete of a non-configurable property throws.
    assert_eq!(
        throws("'use strict'; var o={}; Object.defineProperty(o,'x',{value:1}); delete o.x"),
        "TypeError"
    );
    // A TypedArray element can't be deleted (returns true for a canonical-invalid index).
    assert_eq!(run("var a=new Int8Array(2); delete a[5]"), "true");
}

#[test]
fn typedarray_from_of_validation() {
    // from/of validate the constructed result and construct the array-like target before reading it.
    assert_eq!(run("Int8Array.from([1,2,3]).join(',')"), "1,2,3");
    assert_eq!(run("Int8Array.of(4,5,6).join(',')"), "4,5,6");
    assert_eq!(run("Uint8Array.from([1,2,3], x=>x*2).join(',')"), "2,4,6");
    // A custom constructor that returns a non-TypedArray is a TypeError.
    assert_eq!(
        throws("var C=function(){return {};}; Int8Array.from.call(C,[1,2])"),
        "TypeError"
    );
    // A throwing @@iterator getter propagates.
    assert_eq!(throws("var s={}; Object.defineProperty(s,Symbol.iterator,{get(){throw new TypeError('x');}}); Int8Array.from(s)"), "TypeError");
}

#[test]
fn regexp_symbol_methods_are_generic() {
    // @@replace / @@split / @@match / @@search operate through `exec` on a generic object, so a
    // fake matcher with a custom `exec` works.
    assert_eq!(
        run("var calls=0; var fake={ exec(s){ calls++; return calls===1?Object.assign(['b'],{index:1,length:1}):null; }, global:true, flags:'g' }; RegExp.prototype[Symbol.replace].call(fake, 'abc', 'X')"),
        "aXc"
    );
    // @@search returns the match index and restores lastIndex.
    assert_eq!(run("/c/[Symbol.search]('abcabc')"), "2");
    assert_eq!(run("/x/[Symbol.search]('abc')"), "-1");
}

#[test]
fn regexp_match_and_matchall() {
    assert_eq!(run("'a1b2c3'.match(/\\d/g).join(',')"), "1,2,3");
    // matchAll yields a lazy RegExp String Iterator whose results carry groups.
    assert_eq!(
        run("[...'a1b2'.matchAll(/(?<d>\\d)/g)].map(m=>m.groups.d).join(',')"),
        "1,2"
    );
    assert_eq!(
        run("Object.prototype.toString.call('x'.matchAll(/x/g))"),
        "[object RegExp String Iterator]"
    );
}

#[test]
fn regexp_split_uses_species_and_captures() {
    assert_eq!(run("'a,b,c'.split(/,/).join('|')"), "a|b|c");
    // Capturing groups are spliced into the result.
    assert_eq!(run("'a1b2c'.split(/(\\d)/).join('|')"), "a|1|b|2|c");
    // A limit truncates the result.
    assert_eq!(run("'a,b,c,d'.split(/,/, 2).length"), "2");
}

#[test]
fn regexp_replace_dollar_substitutions() {
    assert_eq!(
        run("'John Smith'.replace(/(\\w+)\\s(\\w+)/, '$2 $1')"),
        "Smith John"
    );
    assert_eq!(run("'abc'.replace(/b/, \"[$`|$&|$']\")"), "a[a|b|c]c");
    // Named-group substitution.
    assert_eq!(run("'2020'.replace(/(?<y>\\d{4})/, '$<y>!')"), "2020!");
}

#[test]
fn regexp_d_flag_indices() {
    assert_eq!(run("/b/d.exec('abc').indices[0].join(',')"), "1,2");
    assert_eq!(run("'has indices: '+/x/d.hasIndices"), "has indices: true");
    // Named-group indices live on `.indices.groups`.
    assert_eq!(
        run("var m=/(?<a>b)(?<c>d)/d.exec('abd'); m.indices.groups.c.join(',')"),
        "2,3"
    );
    // An unmatched optional group's indices entry is undefined.
    assert_eq!(run("typeof /(a)|(b)/d.exec('b').indices[1]"), "undefined");
}

#[test]
fn string_replace_named_group_callback() {
    // The replacer function receives the named-groups object as its last argument.
    assert_eq!(
        run("'2020-06'.replace(/(?<y>\\d+)-(?<m>\\d+)/, (m,y,mo,off,s,g)=>g.m+'/'+g.y)"),
        "06/2020"
    );
}

#[test]
fn eval_lexical_declarations_do_not_leak() {
    // A sloppy direct eval's `let`/`const`/`class` stay in the eval's own lexical scope.
    assert_eq!(run("eval('let x = 1'); typeof x"), "undefined");
    assert_eq!(run("eval('const y = 1'); typeof y"), "undefined");
    assert_eq!(run("eval('class Z {}'); typeof Z"), "undefined");
    // ...but `var`/function declarations hoist into the caller's variable environment.
    assert_eq!(run("eval('var v = 7'); v"), "7");
    assert_eq!(run("eval('function f(){ return 9; }'); f()"), "9");
}

#[test]
fn eval_var_over_lexical_is_syntax_error() {
    // A direct eval must not hoist a `var` over a like-named lexical binding between it and its
    // variable environment (EvalDeclarationInstantiation).
    assert_eq!(throws("{ let x; { eval('var x;'); } }"), "SyntaxError");
    // A global lexical binding conflicts too.
    assert_eq!(throws("let g; eval('var g;')"), "SyntaxError");
}

#[test]
fn eval_var_arguments_in_parameter_default_throws() {
    // With parameter expressions, `arguments`/params live in a parameter environment the eval's
    // variable environment sits below, so `eval("var arguments")` conflicts.
    assert_eq!(
        throws("function f(p = eval('var arguments')) {} f()"),
        "SyntaxError"
    );
    assert_eq!(
        throws("function f(p = eval('var q'), q) {} f()"),
        "SyntaxError"
    );
    // Without parameter expressions there is a single environment — no conflict.
    assert_eq!(run("function f(a){ eval('var a'); return 1; } f()"), "1");
}

#[test]
fn eval_created_local_bindings_are_deletable() {
    // A `var`/function created by a sloppy eval inside a function may be deleted.
    assert_eq!(
        run("(function(){ eval('var x = 5;'); return delete x; })()"),
        "true"
    );
    // An ordinary declaration is not deletable.
    assert_eq!(
        run("(function(){ var y = 5; return delete y; })()"),
        "false"
    );
}

#[test]
fn eval_global_function_non_definable_is_type_error() {
    // `NaN` is a non-configurable, non-writable global — a global function declaration over it fails.
    assert_eq!(throws("eval('function NaN(){}')"), "TypeError");
}

#[test]
fn eval_new_target_and_super_property() {
    // `new.target` is valid in a direct eval inside an ordinary function...
    assert_eq!(
        run("var t; (function(){ t = eval('new.target'); })(); typeof t"),
        "undefined"
    );
    // ...but a super property with no home object is a SyntaxError.
    assert_eq!(throws("eval('super.x')"), "SyntaxError");
    // A top-level arrow does not supply new.target, so its eval rejects it.
    assert_eq!(
        throws("var f = () => eval('new.target'); f()"),
        "SyntaxError"
    );
}

// --- ES modules ------------------------------------------------------------------------------

/// Evaluate an in-memory module graph. `files[0]` is the entry module; every specifier is matched
/// verbatim against a file key. The entry writes its observable results to `globalThis`, which a
/// follow-up script read returns.
fn run_module(files: &[(&str, &str)], read: &str) -> String {
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let entry = owned[0].clone();
    let table = owned.clone();
    let loader = move |spec: &str, _referrer: &str| table.iter().find(|(k, _)| k == spec).cloned();
    let mut engine = Engine::new();
    match engine
        .eval_module(&entry.1, &entry.0, loader)
        .expect("parse")
    {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("module threw {name}: {message}"),
    }
    match engine.eval(read, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("read threw {name}: {message}"),
    }
}

/// Evaluate an entry module expected to throw during linking/evaluation; returns the error name.
fn module_throws(files: &[(&str, &str)]) -> String {
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let entry = owned[0].clone();
    let table = owned.clone();
    let loader = move |spec: &str, _referrer: &str| table.iter().find(|(k, _)| k == spec).cloned();
    let mut engine = Engine::new();
    match engine
        .eval_module(&entry.1, &entry.0, loader)
        .expect("parse")
    {
        Completion::Value(_) => panic!("expected module to throw"),
        Completion::Throw { name, .. } => name,
    }
}

#[test]
fn module_named_and_default_exports() {
    assert_eq!(
        run_module(
            &[
                (
                    "main",
                    "import def, { a, b as c } from 'dep'; globalThis.r = def + ':' + a + ':' + c;"
                ),
                (
                    "dep",
                    "export const a = 1; export const b = 2; export default 'D';"
                ),
            ],
            "r"
        ),
        "D:1:2"
    );
}

#[test]
fn module_live_bindings() {
    // An imported binding observes the exporter's later mutation.
    assert_eq!(
        run_module(
            &[
                ("main", "import { n, bump } from 'dep'; const before = n; bump(); globalThis.r = before + ',' + n;"),
                ("dep", "export let n = 0; export function bump(){ n++; }"),
            ],
            "r"
        ),
        "0,1"
    );
}

#[test]
fn module_default_expression_self_import() {
    // `export default <expr>` bound to *default*, observed via a self-import.
    assert_eq!(
        run_module(
            &[(
                "main",
                "export default (function f(){ return 7; }); import d from 'main'; globalThis.r = d();",
            )],
            "r"
        ),
        "7"
    );
}

#[test]
fn module_namespace_object() {
    let src = &[(
        "main",
        "import * as ns from 'dep'; globalThis.r = Object.keys(ns).join(',') + '|' + ns[Symbol.toStringTag];",
    ), (
        "dep",
        "export const b = 2; export const a = 1; export default 9;",
    )];
    // Namespace keys are sorted; @@toStringTag is "Module".
    assert_eq!(run_module(src, "r"), "a,b,default|Module");
}

#[test]
fn module_namespace_is_frozen() {
    let src = &[
        ("main", "import * as ns from 'dep'; globalThis.set = Reflect.set(ns, 'a', 5); globalThis.a = ns.a;"),
        ("dep", "export const a = 1;"),
    ];
    assert_eq!(run_module(src, "set"), "false");
    assert_eq!(run_module(src, "a"), "1");
}

#[test]
fn module_circular_imports() {
    // A classic cycle: each module imports a function from the other; functions are hoisted.
    assert_eq!(
        run_module(
            &[
                (
                    "a",
                    "import { b } from 'b'; export function a(){ return 'a'; } globalThis.r = b();"
                ),
                (
                    "b",
                    "import { a } from 'a'; export function b(){ return 'b' + a(); }"
                ),
            ],
            "r"
        ),
        "ba"
    );
}

#[test]
fn module_star_reexport() {
    assert_eq!(
        run_module(
            &[
                (
                    "main",
                    "import { x, y } from 'agg'; globalThis.r = x + ',' + y;"
                ),
                ("agg", "export * from 'one'; export * from 'two';"),
                ("one", "export const x = 10;"),
                ("two", "export const y = 20;"),
            ],
            "r"
        ),
        "10,20"
    );
}

#[test]
fn module_missing_export_is_syntax_error() {
    assert_eq!(
        module_throws(&[
            ("main", "import { nope } from 'dep';"),
            ("dep", "export const yes = 1;"),
        ]),
        "SyntaxError"
    );
}

#[test]
fn module_tdz_across_import() {
    // In a cycle, `dep` (evaluated first) reads `main`'s not-yet-initialized `const A` through a
    // re-export, so the access is a temporal-dead-zone ReferenceError.
    assert_eq!(
        run_module(
            &[
                ("main", "import { B } from 'dep'; export const A = 1;"),
                (
                    "dep",
                    "export { A as B } from 'main'; try { B; globalThis.r = 'no'; } catch (e) { globalThis.r = e.name; }",
                ),
            ],
            "r"
        ),
        "ReferenceError"
    );
}

#[test]
fn super_property_context() {
    // `super` outside a method / field / static block is a SyntaxError (parse error).
    assert!(Engine::new().eval("super.x", false).is_err());
    // A bare `super` (neither property nor call) is always a SyntaxError.
    assert!(Engine::new().eval("function f(){ super }", false).is_err());
    // `super.x` in a plain function (not a method) is a SyntaxError.
    assert!(Engine::new()
        .eval("function f(){ return super.x; }", false)
        .is_err());
    // `super.x` inside a method body parses (it is a super-property context).
    assert!(Engine::new()
        .eval("({ m(){ return super.v; } })", false)
        .is_ok());
    // A class method and a field initializer are also super-property contexts.
    assert!(Engine::new()
        .eval(
            "class C extends Object { m(){ return super.x; } f = super.y; }",
            false
        )
        .is_ok());
}

#[test]
fn array_like_near_integer_limit() {
    // Generic Array methods on an array-like with a huge `length` operate on the bounded working
    // span near the limit without hitting the engine's materialization cap.
    assert_eq!(
        run("var o={length: 2**53-1, '9007199254740990':'x'}; Array.prototype.pop.call(o); o.length"),
        "9007199254740990"
    );
    assert_eq!(
        run("var o={length: 2**53-2}; Array.prototype.push.call(o, 1); o.length"),
        "9007199254740991"
    );
    assert_eq!(
        run("var o={length: 2**53+2, '9007199254740989':'a','9007199254740990':'b'}; Array.prototype.slice.call(o, 9007199254740989).join(',')"),
        "a,b"
    );
}

#[test]
fn object_to_locale_string() {
    // Object.prototype.toLocaleString delegates to toString.
    assert_eq!(run("({}).toLocaleString()"), "[object Object]");
    assert_eq!(run("[1,2].toLocaleString()"), "1,2");
    assert_eq!(run("(5).toLocaleString.call(5) === (5).toString()"), "true");
    assert_eq!(
        run("var o={toString(){return 'X'}}; o.toLocaleString()"),
        "X"
    );
}

#[test]
fn to_property_key_symbol_result() {
    // ToPropertyKey does ToPrimitive(String) then keeps a Symbol result as a symbol key.
    assert_eq!(
        run("var s=Symbol('k'); var o={}; o[s]=42; var w={[Symbol.toPrimitive](){return s}}; o[w]"),
        "42"
    );
    // A non-symbol key still coerces via toString.
    assert_eq!(run("var o={}; o[{toString(){return 'x'}}]=9; o.x"), "9");
}

#[test]
fn string_from_char_code_touint16() {
    // fromCharCode ToUint16's each argument.
    assert_eq!(run("String.fromCharCode(-1).charCodeAt(0)"), "65535");
    assert_eq!(run("String.fromCharCode(65537).charCodeAt(0)"), "1");
    assert_eq!(run("String.fromCharCode(65).charCodeAt(0)"), "65");
    assert_eq!(run("String.fromCharCode(NaN).charCodeAt(0)"), "0");
}

#[test]
fn object_proto_accessor() {
    // Object.prototype.__proto__ is an accessor over the prototype.
    assert_eq!(run("var p={x:1}; var o={}; o.__proto__=p; o.x"), "1");
    assert_eq!(
        run("var p={}; var o=Object.create(p); o.__proto__===p"),
        "true"
    );
    assert_eq!(run("({}).__proto__===Object.prototype"), "true");
    // The descriptor on Object.prototype is a configurable accessor.
    assert_eq!(
        run("var d=Object.getOwnPropertyDescriptor(Object.prototype,'__proto__'); typeof d.get+','+typeof d.set+','+d.configurable"),
        "function,function,true"
    );
    // Setting a non-object/null value is a silent no-op.
    assert_eq!(
        run("var o={}; o.__proto__=5; Object.getPrototypeOf(o)===Object.prototype"),
        "true"
    );
}

#[test]
fn set_map_brand_checks() {
    // Set.prototype methods reject a Map receiver and vice-versa (distinct [[SetData]]/[[MapData]]).
    assert_eq!(
        run("try{Set.prototype.forEach.call(new Map(),()=>{});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("try{Set.prototype.clear.call(new Map());'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("try{Set.prototype.union.call(new Map(),new Set());'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("try{Map.prototype.entries.call(new Set());'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // Same-kind still works.
    assert_eq!(
        run("var s=new Set([1,2]); var n=0; s.forEach(v=>n+=v); n"),
        "3"
    );
    assert_eq!(
        run("[...new Set([1,2]).union(new Set([2,3]))].join(',')"),
        "1,2,3"
    );
}

#[test]
fn promise_internal_function_shapes() {
    // The internal resolve/reject functions are anonymous built-ins (own name "", length 1).
    assert_eq!(
        run("var f; new Promise(function(res,rej){f=res}); f.name+','+f.length"),
        ",1"
    );
    // Their name/length are own, non-enumerable, configurable data properties.
    assert_eq!(
        run("var f; new Promise(function(res){f=res}); var d=Object.getOwnPropertyDescriptor(f,'name'); d.value+','+d.enumerable+','+d.configurable"),
        ",false,true"
    );
    // Promise.all element resolve function: name "" length 1.
    assert_eq!(
        run("var order=[]; var thenable={then(res){order.push(res)}}; Promise.all([thenable]); var f=order[0]; f.name+','+f.length"),
        ",1"
    );
}

#[test]
fn iterator_helpers_require_object_this() {
    // Iterator.prototype helpers throw TypeError when `this` is not an object (GetIteratorDirect).
    for m in ["map", "filter", "take", "drop", "flatMap"] {
        let src = format!(
            "try{{Iterator.prototype.{m}.call(5, ()=>{{}}); 'no'}}catch(e){{e.constructor.name}}"
        );
        assert_eq!(run(&src), "TypeError", "lazy helper {m}");
    }
    for m in ["forEach", "reduce", "some", "every", "find", "toArray"] {
        let src = format!(
            "try{{Iterator.prototype.{m}.call(5, ()=>{{}}); 'no'}}catch(e){{e.constructor.name}}"
        );
        assert_eq!(run(&src), "TypeError", "eager helper {m}");
    }
}

#[test]
fn new_target_not_leaked_into_nested_native_call() {
    // A native constructor (Function) invoked as a plain function inside an outer `new` must not
    // inherit the outer new.target — its result's prototype stays %Function.prototype%.
    assert_eq!(
        run("function FACTORY(){ this.f = Function('a','return a'); } var o=new FACTORY(); typeof o.f.apply"),
        "function"
    );
    assert_eq!(
        run("function F(){ this.g = Function('a,b','return a+b'); } (new F()).g(2,3)"),
        "5"
    );
}

#[test]
fn typed_array_bytes_per_element_descriptor() {
    // BYTES_PER_ELEMENT is a non-writable, non-enumerable, non-configurable constant on both the
    // constructor and its prototype.
    for (ctor, size) in [
        ("Int8Array", "1"),
        ("Float64Array", "8"),
        ("Uint16Array", "2"),
    ] {
        assert_eq!(run(&format!("{ctor}.BYTES_PER_ELEMENT")), size);
        assert_eq!(
            run(&format!("var d=Object.getOwnPropertyDescriptor({ctor},'BYTES_PER_ELEMENT'); d.writable+','+d.enumerable+','+d.configurable")),
            "false,false,false"
        );
        assert_eq!(
            run(&format!("var d=Object.getOwnPropertyDescriptor({ctor}.prototype,'BYTES_PER_ELEMENT'); d.value+','+d.configurable")),
            format!("{size},false")
        );
    }
}

#[test]
fn date_to_temporal_instant() {
    // A valid Date yields a Temporal.Instant at ms×10^6 ns.
    assert_eq!(
        run("new Date(0).toTemporalInstant().epochMilliseconds"),
        "0"
    );
    assert_eq!(
        run("new Date(1000).toTemporalInstant().epochMilliseconds"),
        "1000"
    );
    // An invalid Date is a RangeError; a non-Date receiver is a TypeError.
    assert_eq!(
        run("try{new Date(NaN).toTemporalInstant();'no'}catch(e){e.constructor.name}"),
        "RangeError"
    );
    assert_eq!(
        run("try{Date.prototype.toTemporalInstant.call({});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn array_length_shrink_stops_at_non_configurable() {
    // Reducing length past a non-configurable element throws and length settles just past it.
    assert_eq!(
        run("var a=[0,1]; Object.defineProperty(a,'1',{configurable:false}); try{Object.defineProperty(a,'length',{value:1});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("var a=[0,1]; Object.defineProperty(a,'1',{configurable:false}); try{a.length=1;}catch(e){} a.length"),
        "2"
    );
    // A normal shrink still works.
    assert_eq!(run("var a=[1,2,3,4]; a.length=2; a.join(',')"), "1,2");
}

#[test]
fn atomics_wait_notify_validation_order() {
    // wait/notify reject a non-Int32/BigInt64 array with TypeError before coercing the index.
    assert_eq!(
        run("var poison={valueOf(){throw new Error('x')}}; try{Atomics.notify(new Float64Array(4), poison);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("try{Atomics.notify(new Int8Array(4), 0);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // wait needs a shared buffer (a non-shared Int32Array is a TypeError).
    assert_eq!(
        run("try{Atomics.wait(new Int32Array(4), 0, 0);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn generator_function_intrinsics() {
    // Each function kind's [[Prototype]] is its own intrinsic whose constructor is the matching
    // dynamic-function constructor (reachable only via the prototype chain).
    assert_eq!(
        run("Object.getPrototypeOf(function*(){}).constructor.name"),
        "GeneratorFunction"
    );
    assert_eq!(
        run("Object.getPrototypeOf(async function(){}).constructor.name"),
        "AsyncFunction"
    );
    assert_eq!(
        run("Object.getPrototypeOf(async function*(){}).constructor.name"),
        "AsyncGeneratorFunction"
    );
    // The intrinsic constructors dynamically compile the right kind of function.
    assert_eq!(run("var GF=Object.getPrototypeOf(function*(){}).constructor; var g=GF('yield 1;'); g().next().value"), "1");
    assert_eq!(run("var AF=Object.getPrototypeOf(async function(){}).constructor; typeof AF('return 1')().then"), "function");
    // @@toStringTag on the prototype objects.
    assert_eq!(
        run("Object.getPrototypeOf(function*(){})[Symbol.toStringTag]"),
        "GeneratorFunction"
    );
    // Still functions (inherit call/apply from %Function.prototype%).
    assert_eq!(run("(function*(){}) instanceof Function"), "true");
}

#[test]
fn shadow_realm_wrapped_function_copies_name_length() {
    // A ShadowRealm WrappedFunction copies the target's name and length.
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('(function fn(a,b){})'); f.name+','+f.length"),
        "fn,2"
    );
    assert_eq!(
        run("var r=new ShadowRealm(); var f=r.evaluate('(function(){})'); var d=Object.getOwnPropertyDescriptor(f,'length'); d.writable+','+d.configurable"),
        "false,true"
    );
}

#[test]
fn map_set_iterators() {
    // Map/Set iterators have the right @@toStringTag and iterate live.
    assert_eq!(
        run("var m=new Map([['a',1],['b',2]]); [...m.entries()].map(e=>e.join(':')).join(',')"),
        "a:1,b:2"
    );
    assert_eq!(
        run("var s=new Set([1,2,3]); [...s.values()].join(',')"),
        "1,2,3"
    );
    assert_eq!(
        run("var m=new Map(); m.entries()[Symbol.toStringTag]"),
        "Map Iterator"
    );
    assert_eq!(
        run("var s=new Set(); s.values()[Symbol.toStringTag]"),
        "Set Iterator"
    );
    // Map iterator next() brand-checks its receiver.
    assert_eq!(
        run("var it=new Map().entries(); try{it.next.call({});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // Entries appended during iteration are observed.
    assert_eq!(run("var m=new Map([[0,0]]); var out=[]; for(var[k]of m){out.push(k); if(k<3)m.set(k+1,0);} out.join(',')"), "0,1,2,3");
}

#[test]
fn throw_type_error_intrinsic() {
    // A strict function's arguments exposes `callee` as the %ThrowTypeError% poison accessor.
    assert_eq!(
        run("var a=(function(){'use strict';return arguments})(); var d=Object.getOwnPropertyDescriptor(a,'callee'); typeof d.get+','+(d.get===d.set)+','+d.configurable"),
        "function,true,false"
    );
    // %ThrowTypeError% is a frozen, length-0, empty-named function that throws on call.
    assert_eq!(
        run("var T=Object.getOwnPropertyDescriptor((function(){'use strict';return arguments})(),'callee').get; T.name+','+T.length+','+Object.isExtensible(T)"),
        ",0,false"
    );
    assert_eq!(
        run("var T=Object.getOwnPropertyDescriptor((function(){'use strict';return arguments})(),'callee').get; try{T();'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn generator_prototype_chain() {
    // A generator function's .prototype chains to %GeneratorPrototype% ("Generator").
    assert_eq!(
        run("Object.getPrototypeOf(function*(){}.prototype)[Symbol.toStringTag]"),
        "Generator"
    );
    // An async generator function has a .prototype whose chain reaches %AsyncIteratorPrototype%.
    assert_eq!(run("typeof (async function*(){}).prototype"), "object");
    assert_eq!(
        run("var p=Object.getPrototypeOf(Object.getPrototypeOf((async function*(){}).prototype)); typeof p[Symbol.asyncIterator]"),
        "function"
    );
    // %AsyncIteratorPrototype%[@@asyncIterator] returns this.
    assert_eq!(
        run("var P=Object.getPrototypeOf(Object.getPrototypeOf((async function*(){}).prototype)); var o={}; Object.setPrototypeOf(o,P); o[Symbol.asyncIterator]()===o"),
        "true"
    );
}

#[test]
fn proxy_set_receiver_and_strict_delete() {
    // A missing/null `set` trap forwards to the target's [[Set]] with the original Receiver, so a
    // target setter sees `this` === the proxy.
    assert_eq!(
        run("var ctx; var t={set attr(v){ctx=this}}; var p=new Proxy(t,{set:null}); p.attr=1; ctx===p"),
        "true"
    );
    // A strict `delete` through a proxy whose [[Delete]] returns false throws a TypeError.
    assert_eq!(
        run("'use strict'; var f=function(){}; var p=new Proxy(new Proxy(f,{}),{}); try{delete p.prototype;'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // Object.keys forwards ownKeys + enumerability through a proxy target.
    assert_eq!(
        run("var o={a:1,b:2}; var p=new Proxy(new Proxy(o,{}),{ownKeys:null}); Object.keys(p).join(',')"),
        "a,b"
    );
}

#[test]
fn function_bind_length_and_tostring() {
    // bind length: max(0, ToInteger(own length) - boundArgs); only own Number lengths count.
    assert_eq!(run("function f(a,b,c){}; f.bind().length"), "3");
    assert_eq!(run("function f(a,b,c){}; f.bind(null,1).length"), "2");
    assert_eq!(
        run("var f=function(){}; Object.defineProperty(f,'length',{value:NaN}); f.bind().length"),
        "0"
    );
    assert_eq!(run("var f=function(){}; Object.defineProperty(f,'length',{value:Infinity}); f.bind(null,1).length"), "Infinity");
    // Function.prototype.toString throws for a non-callable receiver.
    assert_eq!(
        run("try{Function.prototype.toString.call({});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn string_replace_all_spec_order() {
    // A non-global regexp searchValue is a TypeError.
    assert_eq!(
        run("try{'aaa'.replaceAll(/a/,'b');'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // A global regexp routes through @@replace.
    assert_eq!(run("'a1b1c'.replaceAll(/1/g,'X')"), "aXbXc");
    // A primitive searchValue's Symbol.replace is never accessed.
    assert_eq!(run("'a1b1c'.replaceAll(1,'X')"), "aXbXc");
    // String search still works.
    assert_eq!(run("'a.b.c'.replaceAll('.','-')"), "a-b-c");
}

#[test]
fn string_match_search_delegate() {
    // match/search build a RegExp from a non-regexp arg and go through @@match/@@search.
    assert_eq!(run("'abc123'.match(/[0-9]+/)[0]"), "123");
    assert_eq!(run("'abc'.match('b')[0]"), "b");
    assert_eq!(run("'abcdef'.search('cd')"), "2");
    assert_eq!(run("'abcdef'.search(/xy/)"), "-1");
    // An object regexp with a custom @@search is honored.
    assert_eq!(run("'x'.search({[Symbol.search](s){return 42}})"), "42");
    assert_eq!(run("'x'.match({[Symbol.match](s){return 'M'}})"), "M");
}

#[test]
fn string_split_delegate() {
    // split builds through @@split for regexps and honors a custom @@split.
    assert_eq!(run("'a,b,c'.split(',').join('|')"), "a|b|c");
    assert_eq!(run("'a1b2c'.split(/[0-9]/).join('|')"), "a|b|c");
    assert_eq!(run("'x'.split({[Symbol.split](s){return ['S']}})[0]"), "S");
    assert_eq!(run("'abc'.split('').join('-')"), "a-b-c");
}

#[test]
fn proxy_get_receiver() {
    // A missing `get` trap forwards to the target's [[Get]] with the original Receiver, so a target
    // getter's `this` is the proxy (or the inheriting object), not the target.
    assert_eq!(
        run("var t={get attr(){return this}}; var p=new Proxy(t,{}); p.attr===p"),
        "true"
    );
    assert_eq!(
        run("var t={get attr(){return this}}; var pp=Object.create(new Proxy(t,{})); pp.attr===pp"),
        "true"
    );
    // Reflect.get with an explicit receiver threads it through the proxy.
    assert_eq!(
        run("var t={get a(){return this.v}}; var p=new Proxy(t,{}); Reflect.get(p,'a',{v:9})"),
        "9"
    );
}

#[test]
fn proxy_for_in_and_has_own() {
    // for-in over a proxy enumerates via [[OwnPropertyKeys]] + enumerable, through a proxy target.
    assert_eq!(
        run("var o={a:1,b:2}; var p=new Proxy(new Proxy(o,{}),{}); var out=[]; for(var k in p)out.push(k); out.sort().join(',')"),
        "a,b"
    );
    // hasOwnProperty + propertyIsEnumerable go through the proxy's [[GetOwnProperty]].
    assert_eq!(
        run("var o={a:1}; var p=new Proxy(o,{}); Object.prototype.hasOwnProperty.call(p,'a')"),
        "true"
    );
    assert_eq!(
        run("var o={a:1}; var p=new Proxy(o,{}); p.propertyIsEnumerable('a')"),
        "true"
    );
    assert_eq!(
        run(
            "var o={a:1}; var p=new Proxy(o,{}); Object.getOwnPropertyDescriptor(p,'a').enumerable"
        ),
        "true"
    );
}

#[test]
fn proxy_has_string_wrapper_and_symbol_key() {
    // `in`/Reflect.has forward a String wrapper's exotic length/index through a proxy target.
    assert_eq!(run("'length' in new String('str')"), "true");
    assert_eq!(
        run("0 in new Proxy(new Proxy(new String('str'),{}),{})"),
        "true"
    );
    // The has trap receives the original property key: a symbol stays a symbol.
    assert_eq!(
        run("var s=Symbol(); var t=new Proxy({},{has(_,k){return k===s}}); var p=new Proxy(t,{}); Reflect.has(p,s)"),
        "true"
    );
}

#[test]
fn proxy_define_property_invariants() {
    // A trap can't report a non-configurable target property as configurable.
    assert_eq!(
        run("var t={}; Object.defineProperty(t,'foo',{value:1,configurable:false}); var p=new Proxy(t,{defineProperty(){return true}}); try{Object.defineProperty(p,'foo',{value:1,configurable:true});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // A non-configurable writable data target can't be reported non-writable (step 16.c).
    assert_eq!(
        run("var p=new Proxy({},{defineProperty(t,k){Object.defineProperty(t,k,{configurable:false,writable:true});return true}}); try{Reflect.defineProperty(p,'x',{writable:false});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn set_returns_boolean() {
    // [[Set]] reports failure as a boolean (Reflect.set / proxy trap), while an assignment throws.
    assert_eq!(
        run("var o={get x(){return 1}}; Reflect.set(o,'x',2)"),
        "false"
    );
    assert_eq!(
        run("Reflect.set(new Proxy(new Proxy(/x/g,{}),{}),'global',true)"),
        "false"
    );
    assert_eq!(
        run("var o={a:1}; Reflect.set(new Proxy(o,{}),'a',2)"),
        "true"
    );
    assert_eq!(
        run("Object.freeze({}); var o=Object.freeze({b:1}); Reflect.set(o,'b',9)"),
        "false"
    );
    // A strict assignment to a getter-only property still throws.
    assert_eq!(
        run("'use strict'; var o={get x(){}}; try{o.x=1;'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn proxy_get_set_symbol_trap_key() {
    // get/set traps receive the original symbol key, not a stringified form.
    assert_eq!(
        run("var s=Symbol(); var t=new Proxy({},{get(_,k){return k===s?42:0}}); var p=new Proxy(t,{get:null}); p[s]"),
        "42"
    );
    assert_eq!(
        run("var s=Symbol(); var got; var p=new Proxy({},{set(_,k,v){got=(k===s);return true}}); p[s]=1; String(got)"),
        "true"
    );
    // String-wrapper length/index forward through a nested proxy's [[Get]].
    assert_eq!(
        run("var p=new Proxy(new Proxy(new String('str'),{}),{get:null}); p.length+','+p[0]"),
        "3,s"
    );
}

#[test]
fn array_buffer_slice_and_transfer_detach() {
    // transfer detaches the source; slicing a detached buffer throws TypeError.
    assert_eq!(
        run("var s=new ArrayBuffer(4); var d=s.transfer(5); s.byteLength+','+d.byteLength"),
        "0,5"
    );
    assert_eq!(run("var s=new ArrayBuffer(4); s.transfer(); try{s.slice();'no'}catch(e){e.constructor.name}"), "TypeError");
    // slice requires an ArrayBuffer receiver and rejects a SharedArrayBuffer.
    assert_eq!(
        run("try{ArrayBuffer.prototype.slice.call({});'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // A normal slice copies the range.
    assert_eq!(run("var b=new ArrayBuffer(4); new Uint8Array(b).set([1,2,3,4]); [...new Uint8Array(b.slice(1,3))].join(',')"), "2,3");
}

#[test]
fn array_buffer_slice_species_and_isview() {
    // slice goes through SpeciesConstructor and validates it.
    assert_eq!(run("var b=new ArrayBuffer(4); b.constructor={[Symbol.species]:5}; try{b.slice();'no'}catch(e){e.constructor.name}"), "TypeError");
    assert_eq!(run("var b=new ArrayBuffer(4); b.constructor={[Symbol.species]:function(){}}; try{b.slice();'no'}catch(e){e.constructor.name}"), "TypeError");
    // A custom species is honored.
    assert_eq!(run("var b=new ArrayBuffer(4); var C=function(n){return new ArrayBuffer(n)}; C[Symbol.species]=C; b.constructor=C; b.slice(0,2).byteLength"), "2");
    // isView recognizes DataViews.
    assert_eq!(
        run("ArrayBuffer.isView(new DataView(new ArrayBuffer(8)))"),
        "true"
    );
    assert_eq!(
        run("ArrayBuffer.isView(new Int8Array(4))+','+ArrayBuffer.isView({})"),
        "true,false"
    );
}

#[test]
fn array_buffer_species_and_transfer_resizable() {
    // ArrayBuffer[@@species] returns `this`.
    assert_eq!(run("ArrayBuffer[Symbol.species]===ArrayBuffer"), "true");
    // transfer preserves the source's resizability; transferToFixedLength does not.
    assert_eq!(
        run("var b=new ArrayBuffer(4,{maxByteLength:8}); b.transfer(6).resizable"),
        "true"
    );
    assert_eq!(
        run("var b=new ArrayBuffer(4,{maxByteLength:8}); b.transferToFixedLength(6).resizable"),
        "false"
    );
    assert_eq!(
        run("var b=new ArrayBuffer(4); b.transfer().resizable"),
        "false"
    );
}

#[test]
fn shared_array_buffer_slice_species() {
    // SAB slice requires a SharedArrayBuffer, goes through species, and copies the range.
    assert_eq!(run("var s=new SharedArrayBuffer(4); new Uint8Array(s).set([1,2,3,4]); [...new Uint8Array(s.slice(1,3))].join(',')"), "2,3");
    assert_eq!(run("try{SharedArrayBuffer.prototype.slice.call(new ArrayBuffer(4));'no'}catch(e){e.constructor.name}"), "TypeError");
    assert_eq!(run("var s=new SharedArrayBuffer(4); s.constructor={[Symbol.species]:5}; try{s.slice();'no'}catch(e){e.constructor.name}"), "TypeError");
    assert_eq!(
        run("SharedArrayBuffer[Symbol.species]===SharedArrayBuffer"),
        "true"
    );
}

#[test]
fn array_iteration_uses_toobject_receiver() {
    // Array.prototype.map.call(primitive, cb): the callback's `this`-object arg is ToObject(this),
    // i.e. a wrapper, not the raw primitive.
    assert_eq!(
        run("Boolean.prototype[0]=true;Boolean.prototype.length=1;String(Array.prototype.map.call(false,function(v,i,o){return o instanceof Boolean}))"),
        "true"
    );
    // find/some/every throw TypeError on a non-callable predicate even for empty array-likes.
    assert_eq!(
        run("try{[].find(1);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
}

#[test]
fn array_flat_flatmap_species_and_throw() {
    // flat/flatMap honor ArraySpeciesCreate and CreateDataPropertyOrThrow.
    assert_eq!(run("[1,[2,[3]]].flat().join(',')"), "1,2,3");
    assert_eq!(run("[1,[2,[3]]].flat(2).join(',')"), "1,2,3");
    assert_eq!(
        run("[1,2].flatMap(function(x){return [x,x*2]}).join(',')"),
        "1,2,2,4"
    );
    assert_eq!(run("[1,[2]].flat(Infinity).length"), "2");
    // Non-extensible species result -> CreateDataPropertyOrThrow throws.
    assert_eq!(run("var a=[1];a.constructor={[Symbol.species]:function(){var o=[];Object.preventExtensions(o);return o}};try{a.flat();'no'}catch(e){e.constructor.name}"), "TypeError");
}

#[test]
fn array_species_create_constructor_validation() {
    // A null/primitive `constructor` is not undefined -> IsConstructor check fails -> TypeError.
    assert_eq!(
        run("var a=[1];a.constructor=null;try{a.map(x=>x);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    assert_eq!(
        run("var a=[1];a.constructor=42;try{a.filter(x=>true);'no'}catch(e){e.constructor.name}"),
        "TypeError"
    );
    // A species of null falls back to the default Array.
    assert_eq!(
        run("var a=[1,2];a.constructor={[Symbol.species]:null};a.map(x=>x).length"),
        "2"
    );
    // undefined constructor -> default Array (no throw).
    assert_eq!(
        run("var a=[1,2];a.constructor=undefined;a.map(x=>x+1).join(',')"),
        "2,3"
    );
}

#[test]
fn array_species_result_uses_create_data_prop_or_throw() {
    // map/filter/concat/splice write results via CreateDataPropertyOrThrow: a non-extensible
    // species result makes the write throw a TypeError.
    let mk = |m: &str| {
        format!(
            "var a=[1,2,3];a.constructor={{[Symbol.species]:function(){{var o=[];Object.preventExtensions(o);return o}}}};try{{a.{m};'no'}}catch(e){{e.constructor.name}}"
        )
    };
    assert_eq!(run(&mk("map(x=>x)")), "TypeError");
    assert_eq!(run(&mk("filter(x=>true)")), "TypeError");
    assert_eq!(run(&mk("splice(0,1)")), "TypeError");
    assert_eq!(run(&mk("concat([4])")), "TypeError");
}

#[test]
fn array_from_async_getmethod_and_arraylike() {
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    // Array.fromAsync on a non-iterable primitive ToObjects it -> empty array (no throw).
    assert_eq!(
        two(
            "globalThis.r='x';Array.fromAsync(5).then(a=>{globalThis.r=a.length})",
            "r"
        ),
        "0"
    );
    // A present-but-non-callable @@iterator is a GetMethod TypeError -> promise rejects.
    assert_eq!(
        two(
            "globalThis.r='x';var o={};o[Symbol.iterator]=true;Array.fromAsync(o).catch(e=>{globalThis.r=e.constructor.name})",
            "r"
        ),
        "TypeError"
    );
}

#[test]
fn super_call_in_ordinary_function_is_early_error() {
    // A super() call in a function/generator/async(-generator) that is not a derived constructor
    // is an early SyntaxError.
    assert!(Engine::new()
        .eval("(function(){ super(); })", false)
        .is_err());
    assert!(Engine::new()
        .eval("(function*(){ super(); })", false)
        .is_err());
    assert!(Engine::new()
        .eval("(async function*(){ super(); })", false)
        .is_err());
    // A derived-class constructor's super() is still valid.
    assert_eq!(
        run("class B{constructor(){this.v=1}}class D extends B{constructor(){super()}}new D().v"),
        "1"
    );
    // A nested arrow inherits, a nested class constructor is its own context (both fine).
    assert_eq!(
        run("class B{constructor(){this.v=2}}class D extends B{constructor(){(()=>super())()}}new D().v"),
        "2"
    );
}

#[test]
fn async_generator_yield_star_delegation() {
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    // yield* over a sync iterable inside an async generator, collected async.
    assert_eq!(
        two(
            "globalThis.out=[];async function* g(){yield* [1,2,3]}async function run(){for await(var x of g())globalThis.out.push(x)}run().then(()=>{globalThis.out=globalThis.out.join(',')})",
            "out"
        ),
        "1,2,3"
    );
    // yield* over an inner async generator.
    assert_eq!(
        two(
            "globalThis.out=[];async function* inner(){yield 'a';yield 'b'}async function* g(){yield* inner();yield 'c'}async function run(){for await(var x of g())globalThis.out.push(x)}run().then(()=>{globalThis.out=globalThis.out.join(',')})",
            "out"
        ),
        "a,b,c"
    );
}

#[test]
fn async_generator_yield_awaits_operand() {
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    // yield Promise.reject(x) -> the awaited rejection rejects next().
    assert_eq!(
        two(
            "globalThis.r='x';async function* g(){yield Promise.reject('boom')}var it=g();it.next().then(()=>{globalThis.r='resolved'},e=>{globalThis.r='rej:'+e})",
            "r"
        ),
        "rej:boom"
    );
    // yield of a fulfilled promise unwraps to its value.
    assert_eq!(
        two(
            "globalThis.r='x';async function* g(){yield Promise.resolve(42)}var it=g();it.next().then(v=>{globalThis.r=v.value})",
            "r"
        ),
        "42"
    );
}

#[test]
fn generator_prototype_constructor_links() {
    // %Generator%/%AsyncGenerator% (the function .prototype) <-> their instance prototype.
    assert_eq!(run("function* g(){}Object.getPrototypeOf(g).prototype===Object.getPrototypeOf(g.prototype)"), "true");
    assert_eq!(
        run("function* g(){}g.prototype.constructor===Object.getPrototypeOf(g)"),
        "true"
    );
    assert_eq!(
        run("async function* g(){}g.prototype.constructor===Object.getPrototypeOf(g)"),
        "true"
    );
    // The constructor link (on %GeneratorPrototype%) is non-enumerable, non-writable, configurable.
    assert_eq!(run("function* g(){}var d=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(g.prototype),'constructor');[d.writable,d.enumerable,d.configurable].join(',')"), "false,false,true");
}

#[test]
fn get_iterator_reads_next_lazily() {
    // GetIterator only reads `next`; a missing/non-callable `next` fails when called, not at open.
    // Here the pattern completes without ever stepping (empty pattern), so no error occurs.
    assert_eq!(
        run("var it={};var o={[Symbol.iterator](){return it}};var x=([]=o,'ok');x"),
        "ok"
    );
    // Actually stepping a next-less iterator throws a TypeError (next is not a function).
    assert_eq!(
        run("var it={};var o={[Symbol.iterator](){return it}};var n='none';try{var[a]=o}catch(e){n=e.constructor.name}n"),
        "TypeError"
    );
}

#[test]
fn super_assignment_null_base_throws() {
    // `super.x = v` with a null home-object prototype: ToObject(super base) throws TypeError,
    // but only after the RHS is evaluated.
    assert_eq!(
        run("var count=0;class C{static m(){super.x=(count+=1)}}Object.setPrototypeOf(C,null);var n='none';try{C.m()}catch(e){n=e.constructor.name}n+':'+count"),
        "TypeError:1"
    );
}

#[test]
fn assignment_to_tdz_binding_throws() {
    // Assigning to a let/const still in its temporal dead zone is a ReferenceError.
    assert_eq!(throws("(function(){ x = 1; let x; })()"), "ReferenceError");
    assert_eq!(
        throws("(function(){ ({x} = {x:1}); let x; })()"),
        "ReferenceError"
    );
    assert_eq!(
        throws("(function(){ [x] = [1]; let x; })()"),
        "ReferenceError"
    );
    assert_eq!(throws("(function(){ x += 1; let x; })()"), "ReferenceError");
}

#[test]
fn destructuring_assignment_target_reference_order() {
    // The destructuring target's Reference is evaluated before the source element is read.
    assert_eq!(
        run("var log='';function tgt(){log+='t';return {set q(v){log+='set'}}}var o={get p(){log+='p'}};({p:tgt().q}=o);log"),
        "tpset"
    );
    // Array element: target reference before the iterator step.
    assert_eq!(
        run("var log='';var it={next(){log+='n';return{done:false,value:1}}};var src={[Symbol.iterator](){return it}};function tgt(){log+='t';return{}}[tgt().x]=src;log"),
        "tn"
    );
}

#[test]
fn object_rest_destructuring_assignment() {
    // Rest copies own enumerable properties (CopyDataProperties): symbols included, spec key order.
    assert_eq!(
        run("var s=Symbol('x');var o={2:'b',a:1};o[s]=9;var r;({...r}=o);Object.keys(r).join(',')+'|'+(r[s]===9)"),
        "2,a|true"
    );
    // Rest of a string primitive copies its index properties.
    assert_eq!(run("var r;({...r}='hi');r[0]+r[1]"), "hi");
    // Rest target may be a member expression (valid destructuring-assignment target).
    assert_eq!(
        run("var host={};var v={x:1,y:2};({...host.rest}=v);host.rest.x+','+host.rest.y"),
        "1,2"
    );
    // A rest that is not the last property is an early SyntaxError.
    assert!(Engine::new().eval("var a,b;({...a,b}={})", false).is_err());
}

#[test]
fn simple_assignment_reference_before_rhs() {
    // `base[prop()] = rhs()`: the LHS reference (base + key expression) is evaluated before the RHS.
    assert_eq!(
        run("var order='';var b={};function p(){order+='p';return 'k'}function r(){order+='r';return 1}b[p()]=r();order"),
        "pr"
    );
    // Deferred ToPropertyKey: PutValue's ToObject(null) throws TypeError before the key's toString
    // runs (the RHS is still evaluated first, per `=` order).
    assert_eq!(
        run("var hit=false;var k={toString(){hit=true;return 'x'}};var b=null;var name='none';try{b[k]=1}catch(e){name=e.constructor.name}name+':'+hit"),
        "TypeError:false"
    );
    // A member base with a side effect is evaluated once.
    assert_eq!(
        run("var n=0;var o={};function base(){n++;return o}base().x=5;n+':'+o.x"),
        "1:5"
    );
}

#[test]
fn array_destructuring_assignment_iterator_close() {
    // Normal completion with more elements left: IteratorClose runs and a throwing `return`
    // propagates (destructuring throws that error).
    assert_eq!(
        run("var rc=0;var it={next(){return{done:false,value:1}},return(){rc++;throw new Error('x')}};var iter={[Symbol.iterator](){return it}};var _;try{[_]=iter}catch(e){}rc+''"),
        "1"
    );
    // `return` returning a non-object -> TypeError from IteratorClose on normal completion.
    assert_eq!(
        run("var it={next(){return{done:false,value:1}},return(){return 5}};var iter={[Symbol.iterator](){return it}};var _;var name='none';try{[_]=iter}catch(e){name=e.constructor.name}name"),
        "TypeError"
    );
    // A throwing target assignment closes the iterator but keeps the original error.
    assert_eq!(
        run("var rc=0;var it={next(){return{done:false,value:1}},return(){rc++;return{}}};var iter={[Symbol.iterator](){return it}};var name='none';try{[({}).nope.x]=iter}catch(e){name=e.constructor.name}name+':'+rc"),
        "TypeError:1"
    );
}

#[test]
fn compound_assignment_resolves_reference_once() {
    // `with` + compound assignment: the LHS reference is resolved once, so a getter that deletes
    // the binding between GetValue and PutValue still writes back to the original object.
    assert_eq!(
        run("var x=0;var scope={get x(){delete this.x;return 2}};with(scope){x^=3}scope.x"),
        "1"
    );
    // A computed member base is evaluated once (no double side effect).
    assert_eq!(
        run("var n=0;var o={v:5};function base(){n++;return o}base()[('v')]+=1;n+''"),
        "1"
    );
    // Deferred ToPropertyKey: a null base throws TypeError before the key's toString runs.
    assert_eq!(
        run("var hit=false;var k={toString(){hit=true;return 'x'}};var b=null;try{b[k]^=1}catch(e){}String(hit)"),
        "false"
    );
    // Strict PutValue on a deleted global accessor throws ReferenceError.
    assert_eq!(
        throws("'use strict';Object.defineProperty(globalThis,'gx',{configurable:true,get(){delete globalThis.gx;return 2}});gx^=3"),
        "ReferenceError"
    );
}

#[test]
fn slice_nan_end_is_zero() {
    // ToIntegerOrInfinity(NaN) === 0, so a NaN end argument yields an empty slice.
    assert_eq!(run("'abcd'.slice(0, NaN)"), "");
    assert_eq!(run("[1,2,3].slice(0, NaN).length"), "0");
    assert_eq!(
        run("var b=new ArrayBuffer(4); b.slice(0, NaN).byteLength"),
        "0"
    );
    assert_eq!(
        run("var s=new SharedArrayBuffer(8); s.slice(0, NaN).byteLength"),
        "0"
    );
    // Infinite end clamps to the length.
    assert_eq!(run("'abcd'.slice(0, Infinity)"), "abcd");
}
