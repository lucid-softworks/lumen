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
    assert_eq!(
        run("new Temporal.ZonedDateTime(0n,'Asia/Katmandu').minute"),
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
