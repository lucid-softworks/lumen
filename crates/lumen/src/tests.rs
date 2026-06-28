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
fn strict_mode_assignment() {
    assert_eq!(throws("'use strict'; undeclaredStrict = 1;"), "ReferenceError");
}

