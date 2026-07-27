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
fn instanceof_default_intrinsic_and_override() {
    assert_eq!(
        run("function A(){} function B(){} B.prototype=Object.create(A.prototype); var b=new B(); [b instanceof B,b instanceof A,b instanceof Array].join(',')"),
        "true,true,false"
    );
    assert_eq!(
        run("var calls=0; var rhs={[Symbol.hasInstance](v){calls++;return v===7}}; [(7 instanceof rhs),(8 instanceof rhs),calls].join(',')"),
        "true,false,2"
    );
    assert_eq!(
        run("function C(){} var calls=0; var p=new Proxy({}, {getPrototypeOf(){calls++;return C.prototype}}); [(p instanceof C),calls].join(',')"),
        "true,1"
    );
    // Warm the JIT cache, then mutate facts that shapes do and do not encode. Replacing the
    // prototype value preserves A's shape and must still be observed; adding @@hasInstance
    // changes it and must deopt to the user hook.
    assert_eq!(
        run_jit(
            "function A(){} var o=new A();
             function hit(v, C){ return v instanceof C; }
             for(var i=0;i<1000;i++) hit(o,A);
             var before=hit(o,A);
             A.prototype={};
             var after=hit(o,A), calls=0;
             Object.defineProperty(A, Symbol.hasInstance,
               {value:function(v){calls++;return v===o;}, configurable:true});
             [before,after,hit(o,A),calls].join(',')"
        ),
        "true,false,true,1"
    );
}

#[test]
fn jit_constructor_creation_cache_deopts_on_prototype_changes() {
    assert_eq!(
        run_jit(
            "function C(v){ this.x=v; this.y={v:v}; }
             var last;
             for(var i=0;i<1000;i++) last=new C(i);
             var order=Object.keys(last).join(',');
             var hits=0, p={set x(v){hits+=v;}};
             C.prototype=p;
             var changed=new C(7);
             [last.x,last.y.v,order,hits,Object.hasOwn(changed,'x'),changed.y.v].join(':')"
        ),
        "999:999:x,y:7:false:7"
    );
    assert_eq!(
        run_jit(
            "function C(v){this.x=v;}
             for(var i=0;i<1000;i++) new C(i);
             var hits=0;
             Object.defineProperty(C.prototype,'x',{set:function(v){hits+=v;},configurable:true});
             var o=new C(9);
             hits+':'+Object.hasOwn(o,'x')"
        ),
        "9:false"
    );
    // An inherited writable data property still makes OrdinarySet create an own property. This
    // shape is common in prototype-style constructors; changing it to a setter must invalidate
    // the creation proof before the next store.
    assert_eq!(
        run_jit(
            "function C(v){this.x=v;}
             C.prototype.x=0;
             var last;
             for(var i=0;i<1000;i++) last=new C(i);
             var hits=0;
             Object.defineProperty(C.prototype,'x',
               {set:function(v){hits+=v;},configurable:true});
             var changed=new C(9);
             last.x+':'+Object.hasOwn(last,'x')+':'+hits+':'+Object.hasOwn(changed,'x')"
        ),
        "999:true:9:false"
    );
    // Activation-requiring forwarding constructors learn the initialized size dynamically. Their
    // reserved storage must not weaken the same live prototype/descriptor guards.
    assert_eq!(
        run_jit(
            "function C(){
               this.ctor=(arguments.callee===C);
               this.argc=arguments.length;
               this.initialize.apply(this,arguments);
             }
             C.prototype.x=0;
             C.prototype.initialize=function(v){this.x=v;this.y=v+1;};
             var last;
             for(var i=0;i<1000;i++) last=new C(i);
             var hits=0;
             Object.defineProperty(C.prototype,'x',
               {set:function(v){hits+=v;},configurable:true});
             var changed=new C(9);
             [last.ctor,last.argc,last.x,last.y,Object.hasOwn(last,'x'),hits,
              changed.ctor,changed.argc,Object.hasOwn(changed,'x'),changed.y].join(':')"
        ),
        "true:1:999:1000:true:9:true:1:false:10"
    );
    // The activation-aware construct entry must still honor an explicit object return.
    assert_eq!(
        run_jit(
            "function R(){arguments;return {argc:arguments.length};}
             var r;
             for(var i=0;i<1000;i++) r=new R(1,2,3);
             (r instanceof R)+':'+r.argc"
        ),
        "false:3"
    );
    // One base initializer can run against several subclass prototypes. Creation feedback is
    // polymorphic in prototype identity even when every fresh receiver has the same empty shape;
    // mutating one prototype must invalidate all ways before the next assignment.
    assert_eq!(
        run_jit(
            "function Base(v){this.x=v;}
             function A(v){Base.call(this,v)} function B(v){Base.call(this,v)}
             function C(v){Base.call(this,v)} function D(v){Base.call(this,v)}
             A.prototype=Object.create(Base.prototype);
             B.prototype=Object.create(Base.prototype);
             C.prototype=Object.create(Base.prototype);
             D.prototype=Object.create(Base.prototype);
             var cs=[A,B,C,D], last=[];
             for(var i=0;i<1200;i++){var k=i&3;last[k]=new cs[k](i);}
             var seen=0;
             Object.defineProperty(B.prototype,'x',{
               configurable:true,set:function(v){seen+=v;}
             });
             var changed=new B(7), normal=new C(8);
             [last[0].x,last[1].x,last[2].x,last[3].x,seen,
              Object.hasOwn(changed,'x'),normal.x].join(':')"
        ),
        "1196:1197:1198:1199:7:false:8"
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
fn function_apply_dense_and_observable_fallbacks() {
    assert_eq!(run("Math.max.apply(null,[3,7,4])"), "7");
    assert_eq!(
        run("var hits=0,a=[1,2];Object.defineProperty(a,'1',{get(){hits++;return 9}});Math.max.apply(null,a)+','+hits"),
        "9,1"
    );
    assert_eq!(run("Array.prototype[1]=8;Math.max.apply(null,[3,,4])"), "8");
    assert_eq!(
        run("function f(a){arguments[0]=9;return Math.max.apply(null,arguments)+','+a}f(1)"),
        "9,9"
    );
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
fn gc_registry_reuses_dead_object_slots() {
    use std::rc::Rc;

    let (slots_before, _) = crate::value::gc_registry_stats();
    for _ in 0..200_000 {
        drop(crate::value::Object::new(None));
    }
    let (slots_after, free_after) = crate::value::gc_registry_stats();
    assert!(
        slots_after <= slots_before + 1,
        "registry grew with cumulative churn: {slots_before} -> {slots_after}"
    );
    assert!(free_after > 0);

    // A live raw slot must become a strong snapshot handle, then tombstone synchronously when
    // the final owner disappears; the same slot can be reused without retaining the dead RcBox.
    let object = crate::value::Object::new(None);
    let ptr = Rc::as_ptr(&object);
    let snapshot = crate::value::gc_snapshot();
    assert!(snapshot.iter().any(|o| Rc::as_ptr(o) == ptr));
    drop(snapshot);
    drop(object);
    let (slots_final, free_final) = crate::value::gc_registry_stats();
    assert_eq!(slots_final, slots_after);
    assert_eq!(free_final, free_after);
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
fn labelled_continue_while() {
    // Regression: a labelled `continue` targeting a while/do-while used to escape the loop as an
    // uncaught completion and silently terminate the script (issue #4). It must restart the loop.
    assert_eq!(run("var i=0; a: while(i<3){ i++; continue a; } i"), "3");
    assert_eq!(
        run("var i=0; a: do { i++; continue a; } while(i<3); i"),
        "3"
    );
    // Labelled `break` on a while/do-while keeps working.
    assert_eq!(run("var i=0; a: while(i<3){ i++; break a; } i"), "1");
    assert_eq!(run("var i=0; a: do { i++; break a; } while(i<3); i"), "1");
    // Inner while `continue`s the outer label: the outer loop advances, the inner is abandoned.
    assert_eq!(
        run("var log=[]; a: for(var i=0;i<3;i++){ var j=0; while(j<3){ j++; if(j===2) continue a; log.push(i+':'+j);} } log.join(',')"),
        "0:1,1:1,2:1"
    );
    // Labelled continue on an outer while, driven from an inner while.
    assert_eq!(
        run("var n=0; a: while(n<3){ n++; var k=0; while(k<5){ k++; continue a; } } n"),
        "3"
    );
    // Completion value threading: the loop's value is the last non-empty body completion.
    assert_eq!(
        run("var i=0; a: while(i<3){ i++; if(i<3){ i; continue a; } 42; }"),
        "42"
    );
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
fn packed_dense_numeric_array_semantics() {
    let literal = "[0,1,2,3,4,5,6,7,8,9]";
    assert_eq!(
        run(&format!(
            "let a={literal}; delete a[3]; let hole=Object.hasOwn(a,3); \
             a[3]=33; a.push(10); let popped=a.pop(); \
             [hole,a[3],popped,Object.keys(a).join(','),Reflect.ownKeys(a).at(-1)].join('|')"
        )),
        "false|33|10|0,1,2,3,4,5,6,7,8,9|length"
    );
    assert_eq!(
        run(&format!(
            "let a={literal}; Object.defineProperty(a,'8',{{value:88,writable:false,\
             configurable:false,enumerable:true}}); a.length=5; \
             [a.length,a[8],Object.isSealed(Object.seal(a)),Object.isFrozen(Object.freeze(a))].join(',')"
        )),
        "9,88,true,true"
    );
}

#[test]
fn small_holey_arrays_keep_absence_and_prototype_setter_semantics() {
    assert_eq!(
        run("var a=new Array(4); [a.length,0 in a,Object.hasOwn(a,0),Object.keys(a).length,a[0]].join('|')"),
        "4|false|false|0|"
    );
    assert_eq!(
        run("var seen=0; Object.defineProperty(Array.prototype,'0',{set(v){seen=v},configurable:true}); var a=new Array(4); a[0]=7; var out=[seen,Object.hasOwn(a,0),a.length].join('|'); delete Array.prototype[0]; out"),
        "7|false|4"
    );
    assert_eq!(
        run("var a=new Array(4); a[3]=9; a[0]=2; delete a[3]; [a.length,a[0],3 in a,Object.keys(a).join(',')].join('|')"),
        "4|2|false|0"
    );
    assert_eq!(
        run("var a=new Array(4); a.length=2; a.length=4; [2 in a,3 in a,a.length].join('|')"),
        "false|false|4"
    );
    assert_eq!(
        run("var a=new Array(4); Object.preventExtensions(a); a[0]=1; [Object.hasOwn(a,0),a.length].join('|')"),
        "false|4"
    );
}

#[test]
fn packed_elements_do_not_duplicate_far_index_entries() {
    assert_eq!(
        run("var a=[0,1,2,3,4,5,6,7]; a[300]=1; for(var i=8;i<300;i++)a[i]=i; a[300]=2; delete a[300]; var keys=Reflect.ownKeys(a).filter(k=>k==='300').length; [300 in a,keys,a.length].join('|')"),
        "false|0|301"
    );
}

#[test]
fn jit_linked_scan_preserves_loose_htmldda_null_semantics() {
    assert_eq!(
        run_jit(
            "function loose(next){var peek;while((peek=next.link)!=null)next=peek;return [next,peek]}
             function strict(next){var peek;while((peek=next.link)!==null)next=peek;return [next,peek]}
             for(var i=0;i<600;i++){var tail={link:null},head={link:tail};loose(head);strict(head)}
             var dda=$262.IsHTMLDDA;dda.link=null;var head={link:dda};var a=loose(head),b=strict(head);
             [a[0]===head,a[1]===dda,b[0]===dda,b[1]===null].join('|')"
        ),
        "true|true|true|true"
    );
}

#[test]
fn jit_numeric_diamond_fills_small_holey_arrays_and_deopts_for_setters() {
    assert_eq!(
        run_jit(
            "var LIMIT=4;
             function Worker(){this.v=0}
             Worker.prototype.fill=function(packet){var i=0;while(i<LIMIT){this.v++;if(this.v>26)this.v=1;packet.a[i]=this.v;i++}return packet.a.join(',')};
             var w=new Worker,last;for(var n=0;n<600;n++)last=w.fill({a:new Array(4)});
             var seen=0;Object.defineProperty(Array.prototype,'0',{set(v){seen=v},configurable:true});
             var p={a:new Array(4)},out=w.fill(p);delete Array.prototype[0];
             [last,seen,Object.hasOwn(p.a,0),p.a[1],p.a.length,out].join('|')"
        ),
        "5,6,7,8|9|false|10|4|,10,11,12"
    );
}

#[test]
fn jit_scheduler_shell_guards_methods_globals_and_value_types() {
    assert_eq!(
        run_jit(
            "var HELD=4,SUSPENDED=2;
             function Tcb(link,state,id){this.link=link;this.state=state;this.id=id}
             var originalHeld=Tcb.prototype.held=function(){return (this.state&HELD)!=0||(this.state==SUSPENDED)};
             function Scheduler(list){this.list=list;this.current=null;this.seen=0}
             Scheduler.prototype.schedule=function(){
               this.current=this.list;
               while(this.current!=null){
                 if(this.current.held())this.current=this.current.link;
                 else{this.seen=this.current.id;this.current=null}
               }
               return this.seen
             };
             function warmSchedule(s,n){var out=0;for(var i=0;i<n;i++)out=s.schedule();return out}
             var active=new Tcb(null,0,7),held=new Tcb(active,4,3),s=new Scheduler(held);
             var warm=warmSchedule(s,600);
             Tcb.prototype.held=function(){return false};
             s.list=held;var methodChanged=s.schedule();
             Tcb.prototype.held=originalHeld;HELD=0;
             s.list=held;var globalChanged=s.schedule();
             HELD=4;held.state='4';
             s.list=held;var stateChanged=s.schedule();
             held.state=4;Object.setPrototypeOf(held,{held:function(){return false}});
             s.list=held;var protoChanged=s.schedule();
             [warm,methodChanged,globalChanged,stateChanged,protoChanged,s.current===null].join('|')"
        ),
        "7|3|3|7|3|true"
    );
}

#[test]
fn jit_scheduler_active_prefix_materializes_and_deopts_transactionally() {
    assert_eq!(
        run_jit(
            "var HELD=4,SUSPENDED=2,SR=3,RUNNING=0,RUNNABLE=1;
             function Task(){this.last=99}
             Task.prototype.run=function(packet){this.last=packet==null?-1:packet.id;return null};
             function Tcb(link,state,queue,task,id){
               this.link=link;this.state=state;this.queue=queue;this.task=task;this.id=id
             }
             Tcb.prototype.held=function(){return (this.state&HELD)!=0||(this.state==SUSPENDED)};
             var originalRun=Tcb.prototype.run=function(){
               if(this.state==SR){
                 var packet=this.queue;this.queue=packet.link;
                 if(this.queue==null)this.state=RUNNING;else this.state=RUNNABLE
               }else packet=null;
               return this.task.run(packet)
             };
             function Scheduler(list){this.list=list;this.current=null;this.currentId=-1}
             Scheduler.prototype.schedule=function(){
               this.current=this.list;
               while(this.current!=null){
                 if(this.current.held())this.current=this.current.link;
                 else{this.currentId=this.current.id;this.current=this.current.run()}
               }
             };
             function hot(s,t,p,tail,n){
               for(var i=0;i<n;i++){p.link=(i&1)?null:tail;t.state=SR;t.queue=p;s.schedule()}
             }
             var task=new Task(),p={link:null,id:5},tail={link:null,id:7};
             var t=new Tcb(null,SR,p,task,42),gate=new Tcb(t,HELD,null,task,9);
             var s=new Scheduler(gate);hot(s,t,p,tail,600);
             var warm=[task.last,t.state,s.currentId,s.current===null];
             p.link=tail;t.state=SR;t.queue=p;s.schedule();
             var objectLink=[t.queue===tail,t.state,task.last];
             p.link=p;t.state=SR;t.queue=p;s.schedule();
             var selfLink=[t.queue===p,t.state,task.last];
             p.link=undefined;t.state=SR;t.queue=p;s.schedule();
             var undefinedLink=[t.queue===undefined,t.state,task.last];
             var dda=$262.IsHTMLDDA;p.link=dda;t.state=SR;t.queue=p;s.schedule();
             var ddaLink=[t.queue===dda,t.state,task.last];
             p.link=null;SR=99;t.state=3;t.queue=p;s.schedule();
             var globalChanged=[task.last,t.state];
             SR=3;Tcb.prototype.run=function(){this.state=77;return null};
             t.state=SR;t.queue=p;s.schedule();var methodChanged=t.state;
             Tcb.prototype.run=originalRun;var gets=0,sets=0,stored=1;
             Object.defineProperty(t,'queue',{get(){gets++;return p},set(v){sets++;stored=v},configurable:true});
             t.state=SR;p.link=null;s.schedule();
             [warm,objectLink,selfLink,undefinedLink,ddaLink,globalChanged,methodChanged,
              gets,sets,stored===null,t.state,task.last].flat().join('|')"
        ),
        "5|0|42|true|true|1|5|true|1|5|true|0|5|true|0|5|-1|3|77|2|1|true|1|5"
    );
}

#[test]
fn jit_scheduler_active_inline_null_materialization_preserves_stale_owner_aliases() {
    assert_eq!(
        run_jit(
            "var HELD=4,SUSPENDED=2,SR=3,RUNNING=0,RUNNABLE=1;
             function Task(next){this.next=next;this.last=-2}
             Task.prototype.run=function(packet){this.last=packet==null?-1:packet.id;return this.next};
             function Tcb(link,state,queue,task,id){
               this.link=link;this.state=state;this.queue=queue;this.task=task;this.id=id
             }
             Tcb.prototype.held=function(){return (this.state&HELD)!=0||(this.state==SUSPENDED)};
             Tcb.prototype.run=function(){
               if(this.state==SR){
                 var packet=this.queue;this.queue=packet.link;
                 if(this.queue==null)this.state=RUNNING;else this.state=RUNNABLE
               }else packet=null;
               return this.task.run(packet)
             };
             function Scheduler(list){this.list=list;this.current=null;this.currentId=-1}
             Scheduler.prototype.schedule=function(){
               this.current=this.list;
               while(this.current!=null){
                 if(this.current.held())this.current=this.current.link;
                 else{this.currentId=this.current.id;this.current=this.current.run()}
               }
             };
             var tailTask=new Task(null),tail=new Tcb(null,RUNNING,null,tailTask,2);
             var aliasTask=new Task(tail),alias=new Tcb(null,SR,null,aliasTask,1);
             alias.queue=alias;
             var s=new Scheduler(alias);
             for(var i=0;i<600;i++){
               alias.state=SR;alias.queue=alias;aliasTask.next=tail;
               tail.state=RUNNING;tailTask.next=null;s.schedule()
             }
             var aliasResult=[aliasTask.last,tailTask.last,s.currentId,s.current===null];
             var loneTask=new Task(tail),lone=new Tcb(null,SR,{link:null,id:17},loneTask,3);
             tail.state=RUNNING;tailTask.next=null;s.list=lone;s.schedule();
             var lastOwnerResult=[loneTask.last,tailTask.last,lone.state,s.currentId,s.current===null];
             [aliasResult,lastOwnerResult].flat().join('|')"
        ),
        "1|-1|2|true|17|-1|0|2|true"
    );
}

#[test]
fn jit_scheduler_active_null_dispatches_all_richards_roles_and_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active null roles: ' + message;
        }

        // Device's buffered packet moves to a lower-priority held target. Returning the current
        // TCB makes the following null iteration suspend it, while the target retains the owner.
        var deviceScheduler = new Scheduler();
        var deviceTask = new DeviceTask(deviceScheduler);
        var devicePacket = new Packet(null, ID_WORKER, KIND_DEVICE);
        devicePacket.a1 = 91;
        deviceTask.v1 = devicePacket;
        var deviceTarget = new TaskControlBlock(null, ID_WORKER, 1, null, {
          run: function() { throw 'device target ran'; }
        });
        deviceTarget.state = STATE_SUSPENDED | STATE_HELD;
        var device = new TaskControlBlock(
            null, ID_DEVICE_A, 2, null, deviceTask);
        device.state = STATE_RUNNING;
        deviceScheduler.blocks[ID_WORKER] = deviceTarget;
        deviceScheduler.list = device;
        deviceScheduler.schedule();
        check(deviceTask.v1 === null && deviceTarget.queue === devicePacket,
              'Device packet owner moved');
        check(devicePacket.link === null && devicePacket.id === ID_DEVICE_A &&
              devicePacket.a1 === 91 && deviceScheduler.queueCount === 1,
              'Device queue writes');
        check(device.state === STATE_SUSPENDED &&
              deviceTarget.state === (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE) &&
              deviceScheduler.currentId === ID_DEVICE_A &&
              deviceScheduler.currentTcb === null,
              'Device completion');

        // HandlerTask and WorkerTask deliberately have the same three own fields in the same
        // order. A completed Handler work packet must take its queue arm, not Worker's null arm.
        var handlerScheduler = new Scheduler();
        var handlerTask = new HandlerTask(handlerScheduler);
        var handlerWork = new Packet(null, ID_WORKER, KIND_WORK);
        handlerWork.a1 = DATA_SIZE;
        handlerWork.a2[0] = 92;
        handlerTask.v1 = handlerWork;
        var handlerTarget = new TaskControlBlock(null, ID_WORKER, 1, null, {
          run: function() { throw 'handler target ran'; }
        });
        handlerTarget.state = STATE_SUSPENDED | STATE_HELD;
        var handler = new TaskControlBlock(
            null, ID_HANDLER_A, 2, null, handlerTask);
        handler.state = STATE_RUNNING;
        handlerScheduler.blocks[ID_WORKER] = handlerTarget;
        handlerScheduler.list = handler;
        handlerScheduler.schedule();
        check(handlerTask.v1 === null && handlerTarget.queue === handlerWork,
              'Handler work owner moved');
        check(handlerWork.link === null && handlerWork.id === ID_HANDLER_A &&
              handlerWork.a1 === DATA_SIZE && handlerScheduler.queueCount === 1,
              'Handler queue writes');
        check(handler.state === STATE_SUSPENDED &&
              handlerTarget.state === (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE) &&
              handlerScheduler.currentId === ID_HANDLER_A &&
              handlerScheduler.currentTcb === null,
              'Handler completion');

        var workerScheduler = new Scheduler();
        var workerTask = new WorkerTask(workerScheduler, ID_HANDLER_A, 17);
        var worker = new TaskControlBlock(
            null, ID_WORKER, 2, null, workerTask);
        worker.state = STATE_RUNNING;
        workerScheduler.list = worker;
        workerScheduler.schedule();
        check(Object.keys(handlerTask).join('|') ===
              Object.keys(workerTask).join('|') &&
              Object.getPrototypeOf(handlerTask) !== Object.getPrototypeOf(workerTask),
              'Handler and Worker own layouts match');
        check(workerTask.v1 === ID_HANDLER_A && workerTask.v2 === 17,
              'Worker fields untouched');
        check(worker.state === STATE_SUSPENDED &&
              workerScheduler.currentId === ID_WORKER &&
              workerScheduler.currentTcb === null,
              'Worker completion');

        var idleScheduler = new Scheduler();
        var idleTask = new IdleTask(idleScheduler, 23, 1);
        var idle = new TaskControlBlock(null, ID_IDLE, 1, null, idleTask);
        idle.state = STATE_RUNNING;
        idleScheduler.list = idle;
        idleScheduler.schedule();
        check(idleTask.count === 0 && idleTask.v1 === 23,
              'Idle numeric writes');
        check(idle.state === STATE_HELD && idleScheduler.holdCount === 1 &&
              idleScheduler.currentId === ID_IDLE && idleScheduler.currentTcb === null,
              'Idle completion');

        check(device.task === deviceTask && handler.task === handlerTask &&
              worker.task === workerTask && idle.task === idleTask,
              'TCB task owners retained');
        check(deviceScheduler.list === device && handlerScheduler.list === handler &&
              workerScheduler.list === worker && idleScheduler.list === idle,
              'scheduler list owners retained');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_null_dispatch_replays_run_changes_and_task_accessor_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active null role guards: ' + message;
        }
        function oneRole(role) {
          var scheduler = new Scheduler(), task, id;
          if (role === 'Device') {
            task = new DeviceTask(scheduler);
            id = ID_DEVICE_A;
          } else if (role === 'Handler') {
            task = new HandlerTask(scheduler);
            id = ID_HANDLER_A;
          } else if (role === 'Idle') {
            task = new IdleTask(scheduler, 29, 1);
            id = ID_IDLE;
          } else {
            task = new WorkerTask(scheduler, ID_HANDLER_A, 19);
            id = ID_WORKER;
          }
          var tcb = new TaskControlBlock(null, id, 2, null, task);
          tcb.state = STATE_RUNNING;
          scheduler.blocks[id] = tcb;
          scheduler.list = tcb;
          return { scheduler: scheduler, task: task, tcb: tcb, id: id, role: role };
        }
        function checkFinished(one, label) {
          var expectedState = one.role === 'Idle' ? STATE_HELD : STATE_SUSPENDED;
          check(one.tcb.state === expectedState, label + ' final state');
          check(one.scheduler.currentId === one.id &&
                one.scheduler.currentTcb === null, label + ' scheduler completion');
          check(one.tcb.task === one.task && one.scheduler.list === one.tcb,
                label + ' owners retained');
          if (one.role === 'Idle') {
            check(one.task.count === 0 && one.scheduler.holdCount === 1,
                  label + ' Idle effects once');
          }
        }
        function changedRun(role, prototype) {
          var one = oneRole(role);
          var original = prototype.run, hits = 0, sawNull = false;
          var entryState = -1, entryId = -1, wasCurrent = false;
          prototype.run = function(packet) {
            hits++;
            sawNull = packet === null;
            entryState = one.tcb.state;
            entryId = one.scheduler.currentId;
            wasCurrent = one.scheduler.currentTcb === one.tcb;
            return original.call(this, packet);
          };
          one.scheduler.schedule();
          prototype.run = original;
          check(hits === 1 && sawNull && entryState === STATE_RUNNING &&
                entryId === one.id && wasCurrent,
                role + ' changed run source order');
          checkFinished(one, role + ' changed run');
        }

        changedRun('Device', DeviceTask.prototype);
        changedRun('Handler', HandlerTask.prototype);
        changedRun('Idle', IdleTask.prototype);
        changedRun('Worker', WorkerTask.prototype);

        // Changing TCB.task into an accessor changes the receiver shape. The generic replay must
        // publish currentId first, invoke the getter once, and tolerate a further shape mutation
        // performed by the getter before dispatching the returned DeviceTask.
        var accessor = oneRole('Device');
        var storedTask = accessor.task, taskGets = 0, getterState = -1;
        var getterId = -1, getterWasCurrent = false;
        Object.defineProperty(accessor.tcb, 'task', {
          configurable: true,
          get: function() {
            taskGets++;
            getterState = this.state;
            getterId = accessor.scheduler.currentId;
            getterWasCurrent = accessor.scheduler.currentTcb === this;
            this.afterTaskRead = 97;
            return storedTask;
          }
        });
        accessor.scheduler.schedule();
        check(taskGets === 1 && getterState === STATE_RUNNING &&
              getterId === ID_DEVICE_A && getterWasCurrent,
              'task accessor source order');
        check(accessor.tcb.afterTaskRead === 97, 'task getter shape mutation');
        check(accessor.tcb.state === STATE_SUSPENDED &&
              accessor.scheduler.currentTcb === null,
              'task accessor completion');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_pc59_cold_epoch_orders_same_shape_device_and_handler_roles() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'pc59 cold roles: ' + message;
        }

        var deviceScheduler = new Scheduler();
        var deviceTask = new DeviceTask(deviceScheduler);
        var deviceTcb = new TaskControlBlock(
            null, ID_DEVICE_A, 1, null, deviceTask);
        deviceTcb.state = STATE_RUNNING;
        deviceScheduler.list = deviceTcb;

        // Construct both tasks with the exact same own properties, then change only the second
        // task's immediate prototype. Shape alone must not select Device ahead of Handler.
        var handlerScheduler = new Scheduler();
        var handlerTask = new DeviceTask(handlerScheduler);
        Object.setPrototypeOf(handlerTask, HandlerTask.prototype);
        var handlerTcb = new TaskControlBlock(
            null, ID_HANDLER_A, 1, null, handlerTask);
        handlerTcb.state = STATE_RUNNING;
        handlerScheduler.list = handlerTcb;
        var ownLayout = Object.keys(deviceTask).join('|');
        check(ownLayout === Object.keys(handlerTask).join('|') &&
              Object.getPrototypeOf(deviceTask) === DeviceTask.prototype &&
              Object.getPrototypeOf(handlerTask) === HandlerTask.prototype,
              'same own layout, distinct role prototypes');

        // Reject the scheduler shell before it establishes an epoch. pc59 must retain its full
        // exact-method checks when x28 is zero and dispatch each same-shaped task only once.
        var originalHeld = TaskControlBlock.prototype.isHeldOrSuspended;
        var originalDeviceRun = DeviceTask.prototype.run;
        var originalHandlerRun = HandlerTask.prototype.run;
        var heldHits = 0, deviceHits = 0, handlerHits = 0;
        var deviceSawNull = false, handlerSawNull = false;
        var deviceState = -1, handlerState = -1;
        var deviceCurrent = false, handlerCurrent = false;
        var deviceId = -1, handlerId = -1;
        TaskControlBlock.prototype.isHeldOrSuspended = function() {
          heldHits++;
          return originalHeld.call(this);
        };
        DeviceTask.prototype.run = function(packet) {
          deviceHits++;
          deviceSawNull = packet === null;
          deviceState = deviceTcb.state;
          deviceCurrent = deviceScheduler.currentTcb === deviceTcb;
          deviceId = deviceScheduler.currentId;
          return originalDeviceRun.call(this, packet);
        };
        HandlerTask.prototype.run = function(packet) {
          handlerHits++;
          handlerSawNull = packet === null;
          handlerState = handlerTcb.state;
          handlerCurrent = handlerScheduler.currentTcb === handlerTcb;
          handlerId = handlerScheduler.currentId;
          return originalHandlerRun.call(this, packet);
        };

        deviceScheduler.schedule();
        handlerScheduler.schedule();
        TaskControlBlock.prototype.isHeldOrSuspended = originalHeld;
        DeviceTask.prototype.run = originalDeviceRun;
        HandlerTask.prototype.run = originalHandlerRun;

        check(heldHits === 4, 'cold shell method called in source order');
        check(deviceHits === 1 && handlerHits === 1 &&
              deviceSawNull && handlerSawNull,
              'ordered role methods called once');
        check(deviceState === STATE_RUNNING && deviceCurrent &&
              deviceId === ID_DEVICE_A,
              'Device run entry');
        check(handlerState === STATE_RUNNING && handlerCurrent &&
              handlerId === ID_HANDLER_A,
              'Handler run entry');
        check(deviceTcb.state === STATE_SUSPENDED &&
              handlerTcb.state === STATE_SUSPENDED,
              'both roles suspended');
        check(deviceTcb.task === deviceTask && handlerTcb.task === handlerTask &&
              deviceTask.scheduler === deviceScheduler &&
              handlerTask.scheduler === handlerScheduler,
              'task and scheduler owners retained');
        check(deviceScheduler.list === deviceTcb &&
              handlerScheduler.list === handlerTcb &&
              deviceScheduler.currentTcb === null &&
              handlerScheduler.currentTcb === null,
              'cold dispatch completed');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_pc59_device_hold_replays_late_link_throw_and_preserves_owner() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'pc59 Device hold: ' + message;
        }
        function oneIncomingDevice() {
          var scheduler = new Scheduler();
          var device = new DeviceTask(scheduler);
          var marker = { value: 137 };
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          packet.a2[0] = marker;
          var current = new TaskControlBlock(
              null, ID_DEVICE_A, 1, packet, device);
          scheduler.blocks[ID_DEVICE_A] = current;
          scheduler.list = current;
          // Do not return packet: after the Active dequeue, Device.v1 must keep its only fixture
          // owner alive across the throwing fallback.
          return [scheduler, device, current, marker];
        }

        var one = oneIncomingDevice();
        var scheduler = one[0], device = one[1], current = one[2], marker = one[3];
        var originalMark = TaskControlBlock.prototype.markAsHeld;
        var markHits = 0, markState = -1, markCount = -1;
        var markQueueNull = false, markCurrent = false, markOwner = false;
        var linkHits = 0, linkState = -1, linkCount = -1;
        var linkCurrent = false, linkOwner = false;

        // The changed nested method is a late precommit miss after pc59 has selected Device.
        // Generic replay installs the observable link accessor only after Active's dequeue,
        // Device.v1 publication, and holdCount's increment, keeping the original TCB shape hot.
        TaskControlBlock.prototype.markAsHeld = function() {
          markHits++;
          markState = this.state;
          markCount = scheduler.holdCount;
          markQueueNull = this.queue === null;
          markCurrent = scheduler.currentTcb === this &&
                        scheduler.currentId === ID_DEVICE_A;
          markOwner = device.v1 !== null && device.v1.a2[0] === marker;
          Object.defineProperty(this, 'link', {
            configurable: true,
            get: function() {
              linkHits++;
              linkState = this.state;
              linkCount = scheduler.holdCount;
              linkCurrent = scheduler.currentTcb === this;
              linkOwner = device.v1 !== null && device.v1.a2[0] === marker;
              throw 'late link boom';
            }
          });
          return originalMark.call(this);
        };

        var error = '';
        try { scheduler.schedule(); } catch (e) { error = e; }
        TaskControlBlock.prototype.markAsHeld = originalMark;
        check(error === 'late link boom', 'late link throw propagated');
        check(markHits === 1 && markState === STATE_RUNNING && markCount === 1 &&
              markQueueNull && markCurrent && markOwner,
              'mark entry saw prior effects once');
        check(linkHits === 1 && linkState === STATE_HELD && linkCount === 1 &&
              linkCurrent && linkOwner,
              'link getter saw held effects once');
        check(current.queue === null && current.state === STATE_HELD &&
              scheduler.holdCount === 1,
              'Active and hold state retained');
        check(scheduler.currentId === ID_DEVICE_A &&
              scheduler.currentTcb === current && scheduler.list === current,
              'throw stopped outer current assignment');
        check(current.task === device && device.scheduler === scheduler &&
              device.v1 !== null && device.v1.link === null &&
              device.v1.id === ID_WORKER && device.v1.a2[0] === marker &&
              marker.value === 137,
              'packet payload and owners survived');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_active_device_packet_fallback_preserves_graph_and_last_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph Active Device fallback owners: ' + message;
        }
        function oneHold(kind, code) {
          var scheduler = new Scheduler();
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);

          var packet = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          packet.a1 = code;
          packet.a2[0] = { code: code + 1000 };
          if (kind === 'object') {
            packet.link = new Packet(null, ID_DEVICE_B, KIND_DEVICE);
            packet.link.a1 = code + 1;
            packet.link.a2[0] = { code: code + 2000 };
          } else if (kind === 'self') {
            packet.link = packet;
          } else if (kind === 'undefined') {
            packet.link = undefined;
          }
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, packet);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);

          // Preserve the exact six-record graph while ensuring that the single Device hold is
          // the only runnable role. The packet, successor, and payload markers have no roots
          // outside the TCB/task graph after this helper returns.
          for (var id = 0; id < NUMBER_OF_IDS; id++) {
            if (id !== ID_DEVICE_A) scheduler.blocks[id].state = STATE_HELD;
          }
          return [scheduler, scheduler.blocks[ID_DEVICE_A],
                  scheduler.blocks[ID_DEVICE_A].task];
        }

        var nullCase = oneHold('null', 31);
        nullCase[0].holdCount = 1.25;
        nullCase[0].schedule();
        check(nullCase[0].holdCount === 2.25 &&
              nullCase[1].state === STATE_HELD && nullCase[1].queue === null,
              'Null state/IEEE count/queue');
        check(nullCase[2].v1 !== null && nullCase[2].v1.link === null &&
              nullCase[2].v1.a1 === 31 && nullCase[2].v1.a2[0].code === 1031,
              'Null packet last owner');

        var objectCase = oneHold('object', 41);
        objectCase[0].schedule();
        var objectPacket = objectCase[2].v1;
        var objectSuccessor = objectCase[1].queue;
        check(objectCase[0].holdCount === 1 &&
              objectCase[1].state === (STATE_RUNNABLE | STATE_HELD),
              'object state/count');
        check(objectPacket !== null && objectSuccessor !== null &&
              objectPacket.link === objectSuccessor &&
              objectSuccessor.link === null && objectSuccessor.a1 === 42,
              'P.link and C.queue share successor');
        check(objectPacket.a2[0].code === 1041 &&
              objectSuccessor.a2[0].code === 2041,
              'object packet and successor last owners');

        var selfCase = oneHold('self', 51);
        selfCase[0].schedule();
        var selfPacket = selfCase[2].v1;
        check(selfCase[0].holdCount === 1 &&
              selfCase[1].state === (STATE_RUNNABLE | STATE_HELD),
              'self state/count');
        check(selfPacket !== null && selfCase[1].queue === selfPacket &&
              selfPacket.link === selfPacket && selfPacket.a2[0].code === 1051,
              'self P.link/C.queue/Device.v1 owners');

        var undefinedCase = oneHold('undefined', 61);
        undefinedCase[0].schedule();
        check(undefinedCase[0].holdCount === 1 &&
              undefinedCase[1].state === STATE_HELD &&
              undefinedCase[1].queue === undefined,
              'Undefined state/count/queue');
        check(undefinedCase[2].v1 !== null &&
              undefinedCase[2].v1.link === undefined &&
              undefinedCase[2].v1.a2[0].code === 1061,
              'Undefined packet last owner');

        // Device B holds directly into Device A. This exercises Device role routing through
        // generic packet materialization for two consecutive holds.
        function twoHolds() {
          var scheduler = new Scheduler();
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          var packetA = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          var packetB = new Packet(null, ID_DEVICE_B, KIND_DEVICE);
          packetA.a1 = 91; packetB.a1 = 92;
          packetA.a2[0] = { code: 1091 }; packetB.a2[0] = { code: 1092 };
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, packetA);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, packetB);
          for (var id = 0; id < ID_DEVICE_A; id++) scheduler.blocks[id].state = STATE_HELD;
          return [scheduler, scheduler.blocks[ID_DEVICE_A],
                  scheduler.blocks[ID_DEVICE_B],
                  scheduler.blocks[ID_DEVICE_A].task,
                  scheduler.blocks[ID_DEVICE_B].task];
        }
        var pair = twoHolds();
        pair[0].schedule();
        check(pair[0].holdCount === 2 && pair[0].currentId === ID_DEVICE_A &&
              pair[0].currentTcb === null,
              'Device B to A fast graph resume');
        check(pair[1].state === STATE_HELD && pair[2].state === STATE_HELD &&
              pair[1].queue === null && pair[2].queue === null,
              'two Device Active prefixes and holds');
        check(pair[3].v1.a1 === 91 && pair[3].v1.a2[0].code === 1091 &&
              pair[4].v1.a1 === 92 && pair[4].v1.a2[0].code === 1092,
              'two Device packet last owners');
        check(pair[2].link === pair[1] &&
              pair[1].link === pair[0].blocks[ID_HANDLER_B],
              'two Device canonical graph links');

        var cases = [nullCase, objectCase, selfCase, undefinedCase];
        for (var n = 0; n < cases.length; n++) {
          var scheduler = cases[n][0], current = cases[n][1], device = cases[n][2];
          check(scheduler.currentId === ID_DEVICE_A && scheduler.currentTcb === null,
                'current/currentId ' + n);
          check(scheduler.list === scheduler.blocks[ID_DEVICE_B] &&
                scheduler.blocks[ID_DEVICE_B].link === current &&
                current.link === scheduler.blocks[ID_HANDLER_B],
                'graph links ' + n);
          check(current.task === device && device.scheduler === scheduler,
                'task/scheduler owners ' + n);
        }
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_active_device_packet_fallback_replays_live_guards_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph Active Device fallback guards: ' + message;
        }
        function oneHold(code) {
          var scheduler = new Scheduler();
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          var packet = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          packet.a1 = code;
          packet.a2[0] = { code: code + 1000 };
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, packet);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++) {
            if (id !== ID_DEVICE_A) scheduler.blocks[id].state = STATE_HELD;
          }
          return [scheduler, scheduler.blocks[ID_DEVICE_A],
                  scheduler.blocks[ID_DEVICE_A].task];
        }

        var originalRun = DeviceTask.prototype.run;
        var runCase = oneHold(71), runHits = 0, runEntry = '';
        DeviceTask.prototype.run = function(packet) {
          runHits++;
          runEntry = [packet.a1, runCase[1].queue === null,
                      runCase[1].state, runCase[0].currentId,
                      runCase[0].currentTcb === runCase[1], this.v1 === null].join('|');
          return originalRun.call(this, packet);
        };
        runCase[0].schedule();
        DeviceTask.prototype.run = originalRun;
        check(runHits === 1 && runEntry === '71|true|0|4|true|true',
              'changed Device.run once at source entry');
        check(runCase[2].v1.a1 === 71 && runCase[1].state === STATE_HELD &&
              runCase[0].holdCount === 1, 'changed Device.run result');

        var originalHold = Scheduler.prototype.holdCurrent;
        var holdCase = oneHold(72), holdHits = 0, holdEntry = '';
        Scheduler.prototype.holdCurrent = function() {
          holdHits++;
          holdEntry = [holdCase[2].v1.a1, holdCase[1].queue === null,
                       holdCase[1].state, this.holdCount,
                       this.currentId, this.currentTcb === holdCase[1]].join('|');
          return originalHold.call(this);
        };
        holdCase[0].schedule();
        Scheduler.prototype.holdCurrent = originalHold;
        check(holdHits === 1 && holdEntry === '72|true|0|0|4|true',
              'changed holdCurrent once after Device.v1');
        check(holdCase[2].v1.a1 === 72 && holdCase[1].state === STATE_HELD &&
              holdCase[0].holdCount === 1, 'changed holdCurrent result');

        var originalMark = TaskControlBlock.prototype.markAsHeld;
        var markCase = oneHold(73), markHits = 0, markEntry = '';
        TaskControlBlock.prototype.markAsHeld = function() {
          markHits++;
          markEntry = [markCase[2].v1.a1, markCase[1].queue === null,
                       this.state, markCase[0].holdCount,
                       markCase[0].currentId,
                       markCase[0].currentTcb === this].join('|');
          return originalMark.call(this);
        };
        markCase[0].schedule();
        TaskControlBlock.prototype.markAsHeld = originalMark;
        check(markHits === 1 && markEntry === '73|true|0|1|4|true',
              'changed markAsHeld once after count');
        check(markCase[2].v1.a1 === 73 && markCase[1].state === STATE_HELD &&
              markCase[0].holdCount === 1, 'changed markAsHeld result');

        // Observable descriptors must reject eager graph use without being invoked, then execute
        // exactly where the source operation occurs. The stored values retain the only packet
        // owner after each helper's locals disappear.
        var v1Case = oneHold(74), storedV1 = null, v1Gets = 0, v1Sets = 0;
        Object.defineProperty(v1Case[2], 'v1', {
          configurable: true,
          get: function() { v1Gets++; return storedV1; },
          set: function(value) { v1Sets++; storedV1 = value; }
        });
        v1Case[0].schedule();
        check(v1Gets === 0 && v1Sets === 1 && storedV1.a1 === 74 &&
              storedV1.a2[0].code === 1074,
              'Device.v1 descriptor once and last owner');

        var linkCase = oneHold(75), storedLink = linkCase[1].link, linkGets = 0;
        Object.defineProperty(linkCase[1], 'link', {
          configurable: true,
          get: function() { linkGets++; return storedLink; }
        });
        linkCase[0].schedule();
        check(linkGets === 1 && linkCase[2].v1.a1 === 75 &&
              linkCase[1].state === STATE_HELD &&
              linkCase[0].currentTcb === null,
              'TCB.link descriptor once after hold effects');

        var countCase = oneHold(76), count = 0, countGets = 0, countSets = 0;
        Object.defineProperty(countCase[0], 'holdCount', {
          configurable: true,
          get: function() { countGets++; return count; },
          set: function(value) { countSets++; count = value; }
        });
        countCase[0].schedule();
        check(countGets === 1 && countSets === 1 && count === 1 &&
              countCase[2].v1.a1 === 76 && countCase[1].state === STATE_HELD,
              'holdCount descriptor once');

        // A pre-existing v1 alias takes the ordinary Device fallback's overwrite path.
        var aliasCase = oneHold(78), aliasPacket = aliasCase[1].queue;
        aliasCase[2].v1 = aliasPacket;
        aliasCase[0].schedule();
        check(aliasCase[2].v1 === aliasPacket && aliasPacket.a1 === 78 &&
              aliasCase[1].state === STATE_HELD &&
              aliasCase[0].holdCount === 1 && aliasCase[0].currentTcb === null,
              'pre-existing Device.v1 packet alias');

        // A role-local foreign scheduler preserves ordinary Device fallback semantics. Its
        // currentTcb edge is the only fixture owner of the active TCB
        // outside the canonical scheduler graph while holdCurrent executes.
        var foreignCase = oneHold(77), foreign = new Scheduler();
        foreign.currentTcb = foreignCase[1];
        foreignCase[2].scheduler = foreign;
        foreignCase[0].schedule();
        check(foreign.holdCount === 1 && foreign.currentTcb === foreignCase[1] &&
              foreignCase[0].holdCount === 0,
              'foreign scheduler receives hold');
        check(foreignCase[2].v1.a1 === 77 &&
              foreignCase[1].state === STATE_HELD &&
              foreignCase[0].currentId === ID_DEVICE_A &&
              foreignCase[0].currentTcb === null,
              'foreign scheduler ordinary completion');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_active_packet_role_router_exact_roles() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph Active packet roles: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++) scheduler.blocks[id].state = STATE_HELD;
        }

        // Each run retains a complete canonical graph, but publishes exactly one packet-bearing
        // role. Same-layout Worker and Handler records must still select their exact prototypes;
        // Device must not cascade through either role before taking the generic fallback.
        var workerScheduler = new Scheduler();
        addSix(workerScheduler);
        var worker = workerScheduler.blocks[ID_WORKER];
        var workerTask = worker.task;
        var workerPacket = new Packet(null, ID_WORKER, KIND_WORK);
        worker.queue = workerPacket;
        worker.state = STATE_SUSPENDED_RUNNABLE;
        workerScheduler.blocks[ID_HANDLER_B].state = STATE_SUSPENDED | STATE_HELD;
        workerScheduler.list = worker;
        workerScheduler.schedule();
        check(workerTask.v1 === ID_HANDLER_B && workerTask.v2 === DATA_SIZE &&
              workerPacket.id === ID_WORKER && workerPacket.a1 === 0 &&
              workerScheduler.blocks[ID_HANDLER_B].queue === workerPacket,
              'Worker exact packet role');
        check(worker.state === STATE_SUSPENDED && worker.queue === null &&
              workerScheduler.queueCount === 1,
              'Worker Active prefix, queue, and later null suspend');

        var handlerScheduler = new Scheduler();
        addSix(handlerScheduler);
        var handler = handlerScheduler.blocks[ID_HANDLER_A];
        var handlerTask = handler.task;
        var handlerPacket = new Packet(null, ID_HANDLER_A, KIND_DEVICE);
        handler.queue = handlerPacket;
        handler.state = STATE_SUSPENDED_RUNNABLE;
        handlerScheduler.list = handler;
        handlerScheduler.schedule();
        check(handlerTask.v1 === null && handlerTask.v2 === handlerPacket &&
              handlerPacket.link === null,
              'Handler exact incoming role');
        check(handler.state === STATE_SUSPENDED && handler.queue === null &&
              handlerScheduler.holdCount === 0 && handlerScheduler.queueCount === 0,
              'Handler suspended without Worker/Device effects');

        var deviceScheduler = new Scheduler();
        addSix(deviceScheduler);
        var device = deviceScheduler.blocks[ID_DEVICE_A];
        var deviceTask = device.task;
        var devicePacket = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        device.queue = devicePacket;
        device.state = STATE_SUSPENDED_RUNNABLE;
        deviceScheduler.list = device;
        deviceScheduler.schedule();
        check(deviceTask.v1 === devicePacket && devicePacket.link === null &&
              device.state === STATE_HELD && device.queue === null,
              'Device exact hold role');
        check(deviceScheduler.holdCount === 1 && deviceScheduler.queueCount === 0 &&
              deviceScheduler.currentId === ID_DEVICE_A &&
              deviceScheduler.currentTcb === null,
              'Device did not cross-dispatch');

        check(Object.keys(workerTask).join('|') === Object.keys(handlerTask).join('|') &&
              Object.getPrototypeOf(workerTask) !== Object.getPrototypeOf(handlerTask) &&
              deviceTask.scheduler === deviceScheduler,
              'role fixture identities');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_active_device_packet_fallback_parity_case() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function oneHold(kind, code) {
          var scheduler = new Scheduler();
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          var packet = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          packet.a1 = code;
          if (kind === 1) packet.link = new Packet(null, ID_DEVICE_B, KIND_DEVICE);
          if (kind === 2) packet.link = packet;
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, packet);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++) {
            if (id !== ID_DEVICE_A) scheduler.blocks[id].state = STATE_HELD;
          }
          scheduler.schedule();
          var current = scheduler.blocks[ID_DEVICE_A], device = current.task;
          return [scheduler.holdCount, current.state, current.queue === null,
                  device.v1.a1, device.v1.link === current.queue,
                  kind === 2 ? device.v1.link === device.v1 : true,
                  scheduler.currentId, scheduler.currentTcb === null,
                  current.link === scheduler.blocks[ID_HANDLER_B]].join('|');
        }
        oneHold(0, 81) + ';' + oneHold(1, 82) + ';' + oneHold(2, 83)
        "#,
    ]
    .join("\n");
    assert_eq!(
        run_jit(&src),
        "1|4|true|81|true|true|4|true|true;1|5|false|82|true|true|4|true|true;1|5|false|83|true|true|4|true|true"
    );
}

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[test]
fn jit_scheduler_graph_active_packet_role_router_enabled_disabled_parity() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    for router_disabled in [false, true] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::jit_scheduler_graph_active_device_packet_fallback_parity_case")
            .arg("--nocapture")
            .env_remove("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_DEVICE_DIRECT")
            .env_remove("LUMEN_JIT_NO_SCHED_DEVICE_HOLD")
            .env("LUMEN_JIT_REGIONLOG", "1");
        if router_disabled {
            command.env("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH", "1");
        }
        let output = command
            .output()
            .expect("run graph Active packet router parity child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("running 1 test"),
            "graph Active packet router parity child router_disabled={router_disabled} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "active_packet_role_dispatch={}",
                !router_disabled
            )),
            "graph Active packet router parity did not plan the expected gate router_disabled={router_disabled}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn jit_scheduler_trusted_session_rechecks_globals_and_state_after_user_code() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler trusted session: ' + message;
        }

        // The trusted constants are observations from the current schedule() call, not compile-
        // time constants. Changing them before entry must reject the native session before it
        // commits anything, and ordinary execution must consume the packet with the new values.
        var beforePacket = {link: null, id: 71};
        var beforeSeen = null;
        var beforeTcb = new TaskControlBlock(null, ID_WORKER, 1, beforePacket, {
          run: function(packet) { beforeSeen = packet; return null; }
        });
        STATE_SUSPENDED_RUNNABLE = 8;
        STATE_RUNNING = 16;
        STATE_RUNNABLE = 17;
        beforeTcb.state = STATE_SUSPENDED_RUNNABLE;
        var beforeScheduler = new Scheduler();
        beforeScheduler.list = beforeTcb;
        beforeScheduler.schedule();
        check(beforeSeen === beforePacket, 'pre-entry constants replay packet');
        check(beforeTcb.state === 16 && beforeScheduler.currentTcb === null,
              'pre-entry constants replay state');

        STATE_SUSPENDED_RUNNABLE = 3;
        STATE_RUNNING = 0;
        STATE_RUNNABLE = 1;

        // A generic task is arbitrary user code and must end the trusted session. The following
        // TCB therefore has to re-read all three active-state names before interpreting state 8.
        var middlePacket = {link: null, id: 72};
        var middleSeen = null;
        var middleTcb = new TaskControlBlock(null, ID_WORKER, 1, middlePacket, {
          run: function(packet) { middleSeen = packet; return null; }
        });
        var namesMutator = new TaskControlBlock(middleTcb, ID_WORKER, 1, null, {
          run: function() {
            STATE_SUSPENDED_RUNNABLE = 8;
            STATE_RUNNING = 16;
            STATE_RUNNABLE = 17;
            middleTcb.state = STATE_SUSPENDED_RUNNABLE;
            return middleTcb;
          }
        });
        namesMutator.state = STATE_RUNNING;
        var middleScheduler = new Scheduler();
        middleScheduler.list = namesMutator;
        middleScheduler.schedule();
        check(middleSeen === middlePacket, 'post-call constants re-read packet');
        check(middleTcb.state === 16 && middleScheduler.currentTcb === null,
              'post-call constants re-read state');

        STATE_SUSPENDED_RUNNABLE = 3;
        STATE_RUNNING = 0;
        STATE_RUNNABLE = 1;

        // Trusted state is also scoped to the direct continuation. Replace the next TCB's data
        // slot with an accessor from a generic task. Both the active and subsequent suspended
        // iterations must execute the getter, and markAsSuspended must execute the setter once.
        var descriptorScheduler = new Scheduler();
        var descriptorTcb = new TaskControlBlock(
            null, ID_DEVICE_A, 1, null, new DeviceTask(descriptorScheduler));
        descriptorTcb.state = STATE_RUNNING;
        var stateGets = 0, stateSets = 0, storedState = STATE_RUNNING;
        var descriptorMutator = new TaskControlBlock(
            descriptorTcb, ID_WORKER, 1, null, {
              run: function() {
                Object.defineProperty(descriptorTcb, 'state', {
                  configurable: true,
                  get: function() { stateGets++; return storedState; },
                  set: function(value) { stateSets++; storedState = value; }
                });
                return descriptorTcb;
              }
            });
        descriptorMutator.state = STATE_RUNNING;
        descriptorScheduler.list = descriptorMutator;
        descriptorScheduler.schedule();
        check(stateGets === 6 && stateSets === 1,
              'post-call state descriptor invoked exactly');
        check(storedState === STATE_SUSPENDED && descriptorScheduler.currentTcb === null,
              'post-call state descriptor preserved result');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_fast_loop_rechecks_after_generic_calls_and_budget() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler fast loop: ' + message;
        }
        function directDevice(scheduler, link, id) {
          var task = new DeviceTask(scheduler);
          var tcb = new TaskControlBlock(link, id, 1, null, task);
          tcb.state = STATE_RUNNING;
          return tcb;
        }

        // A direct Device suspend enters the internal continuation. The unknown task in the
        // middle must clear it before user code replaces a shell method; the following Device
        // iteration must observe that replacement through ordinary replay.
        var scheduler = new Scheduler();
        var tail = directDevice(scheduler, null, ID_DEVICE_B);
        var originalHeld = TaskControlBlock.prototype.isHeldOrSuspended;
        var heldCalls = 0;
        var mutator = new TaskControlBlock(tail, ID_WORKER, 2, null, {
          run: function() {
            TaskControlBlock.prototype.isHeldOrSuspended = function() {
              heldCalls++;
              return originalHeld.call(this);
            };
            return tail;
          }
        });
        mutator.state = STATE_RUNNING;
        var head = directDevice(scheduler, mutator, ID_DEVICE_A);
        scheduler.list = head;
        scheduler.schedule();
        TaskControlBlock.prototype.isHeldOrSuspended = originalHeld;
        check(heldCalls >= 2, 'generic call invalidates cached method');
        check(head.state === STATE_SUSPENDED && tail.state === STATE_SUSPENDED,
              'both direct devices suspended');
        check(scheduler.currentTcb === null, 'generic chain completed');

        // More than one 1024-transition epoch forces a canonical full-shell re-guard without
        // growing a native frame per iteration or losing any TCB owner.
        var longScheduler = new Scheduler(), chain = null, tcbs = [];
        for (var n = 0; n < 1100; n++) {
          chain = directDevice(longScheduler, chain,
                               (n & 1) ? ID_DEVICE_A : ID_DEVICE_B);
          tcbs.push(chain);
        }
        longScheduler.list = chain;
        longScheduler.schedule();
        var suspended = 0;
        for (var n = 0; n < tcbs.length; n++) {
          if (tcbs[n].state === STATE_SUSPENDED) suspended++;
        }
        check(suspended === tcbs.length, 'budget re-entry preserves every state');
        check(longScheduler.currentTcb === null, 'budget chain completed');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_epoch_rechecks_role_identity_and_task_shape_after_long_direct_chain() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler role epoch: ' + message;
        }
        function directRole(scheduler, link, ordinal) {
          var task, id;
          if ((ordinal % 3) === 0) {
            task = new DeviceTask(scheduler);
            id = ID_DEVICE_A;
          } else if ((ordinal % 3) === 1) {
            task = new HandlerTask(scheduler);
            id = ID_HANDLER_A;
          } else {
            task = new WorkerTask(scheduler, ID_HANDLER_A, 17);
            id = ID_WORKER;
          }
          var tcb = new TaskControlBlock(link, id, 1, null, task);
          tcb.state = STATE_RUNNING;
          return [tcb, task];
        }

        // The first 1040 active-null roles are completely direct, crossing the 1024-transition
        // continuation budget. The next TCB has a task accessor and mutates its own shape while
        // being read; no role fact from the earlier epoch may bypass that source-level get.
        var scheduler = new Scheduler(), tcbs = new Array(1100);
        var tasks = new Array(1100), link = null;
        for (var n = 1099; n >= 0; n--) {
          var pair = directRole(scheduler, link, n);
          tcbs[n] = pair[0];
          tasks[n] = pair[1];
          link = pair[0];
        }
        var accessorIndex = 1040, accessorTcb = tcbs[accessorIndex];
        var accessorTask = tasks[accessorIndex], taskGets = 0;
        var getterState = -1, getterId = -1, getterWasCurrent = false;
        Object.defineProperty(accessorTcb, 'task', {
          configurable: true,
          get: function() {
            taskGets++;
            getterState = this.state;
            getterId = scheduler.currentId;
            getterWasCurrent = scheduler.currentTcb === this;
            this.afterTaskRead = 101;
            return accessorTask;
          }
        });

        // Worker and Handler have identical own layouts, so a later Worker on a distinct
        // prototype is a precise role-prototype guard. Its replacement must run once through
        // ordinary dispatch without affecting the normal roles that follow it.
        var prototypeIndex = 1043, prototypeTcb = tcbs[prototypeIndex];
        var prototypeTask = tasks[prototypeIndex];
        check(prototypeTask instanceof WorkerTask, 'prototype fixture role');
        var alternate = Object.create(WorkerTask.prototype);
        var prototypeHits = 0, prototypeState = -1, prototypeId = -1;
        var prototypeSawNull = false, prototypeWasCurrent = false;
        alternate.run = function(packet) {
          prototypeHits++;
          prototypeState = prototypeTcb.state;
          prototypeId = scheduler.currentId;
          prototypeSawNull = packet === null;
          prototypeWasCurrent = scheduler.currentTcb === prototypeTcb;
          return WorkerTask.prototype.run.call(this, packet);
        };
        Object.setPrototypeOf(prototypeTask, alternate);

        scheduler.list = tcbs[0];
        scheduler.schedule();
        check(taskGets === 1 && getterState === STATE_RUNNING &&
              getterId === accessorTcb.id && getterWasCurrent,
              'post-epoch task getter source order');
        check(accessorTcb.afterTaskRead === 101 &&
              accessorTcb.state === STATE_SUSPENDED,
              'post-epoch task shape mutation');
        check(prototypeHits === 1 && prototypeState === STATE_RUNNING &&
              prototypeId === ID_WORKER && prototypeSawNull && prototypeWasCurrent,
              'post-epoch alternate prototype once');
        check(prototypeTask.v1 === ID_HANDLER_A && prototypeTask.v2 === 17 &&
              prototypeTcb.state === STATE_SUSPENDED,
              'post-epoch Worker result');

        var suspended = 0;
        for (var n = 0; n < tcbs.length; n++) {
          if (tcbs[n].state === STATE_SUSPENDED) suspended++;
        }
        check(suspended === tcbs.length, 'every mixed role suspended once');
        check(tcbs[1023].link === tcbs[1024] && tcbs[1099].link === null,
              'chain owners retained');
        check(tasks[accessorIndex] === accessorTask &&
              tcbs[prototypeIndex].task === prototypeTask,
              'task owners retained');
        check(scheduler.list === tcbs[0] && scheduler.currentTcb === null &&
              scheduler.currentId === tcbs[1099].id,
              'long session completed');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_generic_exit_invalidates_tcb_role_and_shell_global_epoch() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler generic epoch: ' + message;
        }
        function directDevice(scheduler, link, id) {
          var task = new DeviceTask(scheduler);
          var tcb = new TaskControlBlock(link, id, 1, null, task);
          tcb.state = STATE_RUNNING;
          return tcb;
        }

        var scheduler = new Scheduler();
        var tail = directDevice(scheduler, null, ID_DEVICE_B);
        var originalTcbRun = TaskControlBlock.prototype.run;
        var originalDeviceRun = DeviceTask.prototype.run;
        var oldSuspended = STATE_SUSPENDED;
        var tcbHits = 0, tcbState = -1, tcbId = -1, tcbWasCurrent = false;
        var deviceHits = 0, deviceState = -1, deviceId = -1;
        var deviceSawNull = false, deviceWasCurrent = false;

        // This task is deliberately outside every exact role. Its call must terminate the direct
        // session before changing a hoisted TCB method, a role method, and the shell's suspended
        // global. The returned Device must observe all three replacements in this iteration.
        var mutator = new TaskControlBlock(tail, ID_WORKER, 1, null, {
          run: function() {
            TaskControlBlock.prototype.run = function() {
              tcbHits++;
              tcbState = this.state;
              tcbId = scheduler.currentId;
              tcbWasCurrent = scheduler.currentTcb === this;
              return originalTcbRun.call(this);
            };
            DeviceTask.prototype.run = function(packet) {
              deviceHits++;
              deviceState = tail.state;
              deviceId = scheduler.currentId;
              deviceSawNull = packet === null;
              deviceWasCurrent = scheduler.currentTcb === tail;
              return originalDeviceRun.call(this, packet);
            };
            STATE_SUSPENDED = 8;
            return tail;
          }
        });
        mutator.state = STATE_RUNNING;
        var head = directDevice(scheduler, mutator, ID_DEVICE_A);
        scheduler.list = head;
        scheduler.schedule();

        TaskControlBlock.prototype.run = originalTcbRun;
        DeviceTask.prototype.run = originalDeviceRun;
        STATE_SUSPENDED = oldSuspended;
        check(head.state === oldSuspended, 'head used pre-mutation global');
        check(tcbHits === 1 && tcbState === STATE_RUNNING &&
              tcbId === ID_DEVICE_B && tcbWasCurrent,
              'changed TCB.run entered once after currentId');
        check(deviceHits === 1 && deviceState === STATE_RUNNING &&
              deviceId === ID_DEVICE_B && deviceSawNull && deviceWasCurrent,
              'changed Device.run entered once after TCB.run');
        check(tail.state === 8, 'tail used changed suspended global');
        check(scheduler.currentId === ID_DEVICE_B && scheduler.currentTcb === null,
              'changed shell global terminated session');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_refilled_epoch_rechecks_nested_suspend_methods_after_generic_exit() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler nested epoch: ' + message;
        }
        function directDevice(scheduler, link, id) {
          var task = new DeviceTask(scheduler);
          var tcb = new TaskControlBlock(link, id, 1, null, task);
          tcb.state = STATE_RUNNING;
          return tcb;
        }

        var scheduler = new Scheduler();
        var tail = directDevice(scheduler, null, ID_DEVICE_B);
        var tailTask = tail.task;
        var originalSuspend = Scheduler.prototype.suspendCurrent;
        var originalMark = TaskControlBlock.prototype.markAsSuspended;
        var suspendHits = 0, suspendState = -1, suspendId = -1;
        var suspendThis = false, suspendCurrent = false;
        var markHits = 0, markState = -1, markThis = false, markCurrent = false;

        // This unknown task runs only after the direct prefix has crossed and refilled the 1024
        // transition epoch. Replacing both nested methods must invalidate that refilled epoch
        // before the returned Device reaches its null-packet suspend path.
        var mutator = new TaskControlBlock(tail, ID_WORKER, 1, null, {
          run: function() {
            Scheduler.prototype.suspendCurrent = function() {
              suspendHits++;
              suspendState = this.currentTcb.state;
              suspendId = this.currentId;
              suspendThis = this === scheduler;
              suspendCurrent = this.currentTcb === tail;
              return originalSuspend.call(this);
            };
            TaskControlBlock.prototype.markAsSuspended = function() {
              markHits++;
              markState = this.state;
              markThis = this === tail;
              markCurrent = scheduler.currentTcb === this;
              return originalMark.call(this);
            };
            return tail;
          }
        });
        mutator.state = STATE_RUNNING;

        var prefix = new Array(1050), link = mutator;
        for (var n = 1049; n >= 0; n--) {
          prefix[n] = directDevice(
              scheduler, link, (n & 1) ? ID_DEVICE_A : ID_DEVICE_B);
          link = prefix[n];
        }
        scheduler.list = prefix[0];
        scheduler.schedule();

        Scheduler.prototype.suspendCurrent = originalSuspend;
        TaskControlBlock.prototype.markAsSuspended = originalMark;
        check(suspendHits === 1 && suspendState === STATE_RUNNING &&
              suspendId === ID_DEVICE_B && suspendThis && suspendCurrent,
              'changed suspend entered once with live current');
        check(markHits === 1 && markState === STATE_RUNNING &&
              markThis && markCurrent,
              'changed mark entered once after suspend');
        check(tail.state === STATE_SUSPENDED && tail.task === tailTask &&
              tailTask.scheduler === scheduler,
              'tail state and owners');

        var suspended = 0;
        for (var n = 0; n < prefix.length; n++) {
          if (prefix[n].state === STATE_SUSPENDED) suspended++;
        }
        check(suspended === prefix.length, 'direct prefix suspended once');
        check(prefix[1023].link === prefix[1024] &&
              prefix[1049].link === mutator && mutator.link === tail,
              'prefix and tail links retained');
        check(scheduler.list === prefix[0] && scheduler.currentId === ID_DEVICE_B &&
              scheduler.currentTcb === null,
              'refilled session completed');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_foreign_blocks_target_replays_queue_methods_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler foreign target: ' + message;
        }

        var scheduler = new Scheduler();
        var deviceTask = new DeviceTask(scheduler);
        var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
        packet.a1 = 113;
        deviceTask.v1 = packet;
        var source = new TaskControlBlock(
            null, ID_DEVICE_A, 2, null, deviceTask);
        source.state = STATE_RUNNING;

        var target = new TaskControlBlock(null, ID_WORKER, 3, null, {
          run: function() { throw 'held foreign target ran'; }
        });
        target.state = STATE_SUSPENDED | STATE_HELD;
        var ordinary = new TaskControlBlock(null, ID_WORKER, 3, null, {});
        ordinary.state = STATE_SUSPENDED | STATE_HELD;
        var ownLayout = Object.keys(target).join('|');
        check(ownLayout === Object.keys(ordinary).join('|'), 'same own layout fixture');

        // The target comes from Scheduler.blocks and keeps the ordinary TCB own layout, but its
        // immediate prototype overrides both queue methods. A cached standard-TCB method epoch
        // must reject before queue/check writes and replay each foreign method exactly once.
        var foreign = Object.create(TaskControlBlock.prototype);
        var originalCheck = TaskControlBlock.prototype.checkPriorityAdd;
        var originalMark = TaskControlBlock.prototype.markAsRunnable;
        var checkHits = 0, checkTask = null, checkPacket = null;
        var checkCount = -1, checkQueue = 1, checkState = -1, checkId = -1;
        var markHits = 0, markQueue = null, markState = -1;
        foreign.checkPriorityAdd = function(task, value) {
          checkHits++;
          checkTask = task;
          checkPacket = value;
          checkCount = scheduler.queueCount;
          checkQueue = this.queue;
          checkState = this.state;
          checkId = value.id;
          return originalCheck.call(this, task, value);
        };
        foreign.markAsRunnable = function() {
          markHits++;
          markQueue = this.queue;
          markState = this.state;
          return originalMark.call(this);
        };
        Object.setPrototypeOf(target, foreign);
        check(Object.keys(target).join('|') === ownLayout &&
              Object.getPrototypeOf(target) === foreign &&
              target instanceof TaskControlBlock,
              'foreign immediate prototype fixture');

        scheduler.blocks[ID_WORKER] = target;
        scheduler.list = source;
        scheduler.schedule();
        check(checkHits === 1 && checkTask === source && checkPacket === packet &&
              checkCount === 1 && checkQueue === null &&
              checkState === (STATE_SUSPENDED | STATE_HELD) &&
              checkId === ID_DEVICE_A,
              'foreign check saw queue prefix once');
        check(markHits === 1 && markQueue === packet &&
              markState === (STATE_SUSPENDED | STATE_HELD),
              'foreign mark saw target publication once');
        check(deviceTask.v1 === null && source.state === STATE_RUNNING &&
              source.queue === null,
              'source Device effects');
        check(packet.link === null && packet.id === ID_DEVICE_A && packet.a1 === 113 &&
              scheduler.queueCount === 1,
              'packet queue effects');
        check(target.queue === packet &&
              target.state === (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE),
              'foreign target state');
        check(Object.keys(target).join('|') === ownLayout &&
              Object.getPrototypeOf(target) === foreign,
              'foreign target identity retained');
        check(scheduler.currentId === ID_DEVICE_A && scheduler.currentTcb === null &&
              scheduler.list === source && scheduler.blocks[ID_WORKER] === target,
              'scheduler owners and completion');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_epoch_rebuilds_same_layout_blocks_identity_after_delayed_exit() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler graph refill: ' + message;
        }

        // Keep all six entry records canonical. Handler A consumes one TCB queue node per
        // scheduler iteration, so the observable final packet cannot be reached until 1050
        // completely direct active iterations have crossed the 1024-transition refill boundary.
        var scheduler = new Scheduler();
        scheduler.addIdleTask(ID_IDLE, 0, null, 1);
        scheduler.addWorkerTask(ID_WORKER, 1000, null);

        var special = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        special.a1 = 131;
        var queue = special;
        for (var n = 0; n < 1050; n++) {
          var work = new Packet(queue, ID_WORKER, KIND_WORK);
          work.a1 = 0;
          work.a2[0] = 91;
          queue = work;
        }
        scheduler.addHandlerTask(ID_HANDLER_A, 2000, queue);
        scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
        scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
        scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);

        var idle = scheduler.blocks[ID_IDLE];
        var handlerA = scheduler.blocks[ID_HANDLER_A];
        var handlerB = scheduler.blocks[ID_HANDLER_B];
        var original = scheduler.blocks[ID_DEVICE_A];
        var deviceB = scheduler.blocks[ID_DEVICE_B];
        var foreignTask = new DeviceTask(scheduler);
        var foreign = new TaskControlBlock(
            original.link, ID_DEVICE_A, original.priority, null, foreignTask);
        foreign.state = STATE_SUSPENDED | STATE_HELD;
        check(Object.keys(foreign).join('|') === Object.keys(original).join('|') &&
              Object.getPrototypeOf(foreign) === Object.getPrototypeOf(original) &&
              Object.keys(foreignTask).join('|') ===
                  Object.keys(original.task).join('|') &&
              Object.getPrototypeOf(foreignTask) ===
                  Object.getPrototypeOf(original.task),
              'same-layout replacement fixture');

        var kindGets = 0, getterState = -1, getterId = -1;
        var getterWasCurrent = false, getterSawOld = false;
        Object.defineProperty(special, 'kind', {
          configurable: true,
          get: function() {
            kindGets++;
            getterState = handlerA.state;
            getterId = scheduler.currentId;
            getterWasCurrent = scheduler.currentTcb === handlerA;
            getterSawOld = scheduler.blocks[ID_DEVICE_A] === original &&
                           deviceB.link === original;
            // Publish a complete, still-valid six-record graph. A stale epoch pointer would
            // enqueue `special` on the detached original Device A instead of this replacement.
            deviceB.link = foreign;
            scheduler.blocks[ID_DEVICE_A] = foreign;
            return KIND_DEVICE;
          }
        });

        scheduler.schedule();
        check(kindGets === 1 && getterState === STATE_RUNNING &&
              getterId === ID_HANDLER_A && getterWasCurrent && getterSawOld,
              'delayed getter ran once at source position');
        check(scheduler.queueCount === 1 && special.id === ID_HANDLER_A &&
              special.link === null && special.a1 === 91,
              'post-exit queue effects');
        check(foreign.queue === special &&
              foreign.state === (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE) &&
              foreign.task === foreignTask && foreignTask.scheduler === scheduler,
              'replacement record received the packet');
        check(original.queue === null && original.state === STATE_SUSPENDED &&
              original.task.v1 === null,
              'detached record was never reached through a stale pointer');
        check(scheduler.blocks[ID_DEVICE_A] === foreign && deviceB.link === foreign &&
              foreign.link === handlerB && scheduler.list === deviceB,
              'rebuilt six-record owners');
        check(idle.task.scheduler === scheduler && handlerA.task.scheduler === scheduler &&
              scheduler.currentTcb === null && scheduler.currentId === ID_IDLE,
              'refilled session completed');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_epoch_rejects_observable_and_foreign_scheduler_graphs() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'scheduler graph reject: ' + message;
        }
        function addSix(scheduler, idleCount) {
          scheduler.addIdleTask(ID_IDLE, 0, null, idleCount);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
        }

        // An observable blocks property must decline eager graph validation without invoking
        // user code. The one source-level Idle release reads it only after committing count/v1
        // and currentId, which the getter records exactly once.
        var observable = new Scheduler();
        addSix(observable, 2);
        var observableIdle = observable.blocks[ID_IDLE];
        var observableDevice = observable.blocks[ID_DEVICE_B];
        var storedBlocks = observable.blocks;
        var blocksGets = 0, getterCount = -1, getterV1 = -1, getterId = -1;
        var getterWasCurrent = false;
        Object.defineProperty(observable, 'blocks', {
          configurable: true,
          get: function() {
            blocksGets++;
            getterCount = observableIdle.task.count;
            getterV1 = observableIdle.task.v1;
            getterId = this.currentId;
            getterWasCurrent = this.currentTcb === observableIdle;
            return storedBlocks;
          }
        });
        observable.schedule();
        check(blocksGets === 1 && getterCount === 1 && getterV1 === 0xD008 &&
              getterId === ID_IDLE && getterWasCurrent,
              'blocks getter ran once after Idle prefix');
        check(observableIdle.state === STATE_HELD &&
              observableDevice.state === STATE_SUSPENDED &&
              observable.currentTcb === null,
              'observable graph ordinary result');

        // A role task can retain the exact own shape/prototype while its scheduler identity is
        // foreign. Entry validation must reject before calling the foreign method. Drop every
        // outside owner: the task.scheduler edge alone must keep both scheduler and marker alive.
        var mismatch = new Scheduler();
        addSix(mismatch, 1);
        var mismatchDevice = mismatch.blocks[ID_DEVICE_B];
        var mismatchTask = mismatchDevice.task;
        mismatchDevice.state = STATE_RUNNING;
        var foreign = new Scheduler();
        var marker = { code: 223, seen: 0 };
        foreign.marker = marker;
        var methodHits = 0, methodId = -1, methodWasCurrent = false;
        var methodThis = null;
        foreign.suspendCurrent = function() {
          methodHits++;
          methodId = mismatch.currentId;
          methodWasCurrent = mismatch.currentTcb === mismatchDevice;
          methodThis = this;
          this.marker.seen++;
          return null;
        };
        mismatchTask.scheduler = foreign;
        foreign = null;
        marker = null;

        mismatch.schedule();
        check(methodHits === 1 && methodId === ID_DEVICE_B &&
              methodWasCurrent && methodThis === mismatchTask.scheduler,
              'foreign scheduler method ran once at source position');
        check(mismatchTask.scheduler.marker.code === 223 &&
              mismatchTask.scheduler.marker.seen === 1,
              'last-owner scheduler and marker survived rejection');
        check(mismatchDevice.state === STATE_RUNNING && mismatch.currentTcb === null &&
              mismatch.currentId === ID_DEVICE_B,
              'foreign scheduler ordinary result');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_core_suspend_parity_case() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph core suspend: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++)
            scheduler.blocks[id].state = STATE_HELD;
        }

        // One graph session reaches every CORE-backed null suspend consumer in link order:
        // Device A -> held Handler B -> Handler A -> Worker -> held Idle.
        var scheduler = new Scheduler();
        addSix(scheduler);
        var device = scheduler.blocks[ID_DEVICE_A];
        var handler = scheduler.blocks[ID_HANDLER_A];
        var worker = scheduler.blocks[ID_WORKER];
        device.state = STATE_RUNNING;
        handler.state = STATE_RUNNING;
        worker.state = STATE_RUNNING;
        scheduler.list = device;

        scheduler.schedule();

        check(device.state === STATE_SUSPENDED, 'Device suspended');
        check(handler.state === STATE_SUSPENDED, 'Handler suspended');
        check(worker.state === STATE_SUSPENDED, 'Worker suspended');
        check(scheduler.currentId === ID_WORKER && scheduler.currentTcb === null,
              'scheduler completion');
        check(device.task.scheduler === scheduler &&
              handler.task.scheduler === scheduler &&
              worker.task.scheduler === scheduler,
              'task scheduler owners');
        check(device.link === scheduler.blocks[ID_HANDLER_B] &&
              handler.link === worker && worker.link === scheduler.blocks[ID_IDLE],
              'canonical links retained');

        [device.state, handler.state, worker.state, scheduler.currentId,
         scheduler.currentTcb === null, device.task.v1 === null,
         handler.task.v1 === null && handler.task.v2 === null].join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "2|2|2|1|true|true|true");
}

#[test]
fn jit_scheduler_graph_core_epoch_soft_rejects_foreign_task_scheduler_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph core soft reject: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++)
            scheduler.blocks[id].state = STATE_HELD;
        }

        var scheduler = new Scheduler();
        addSix(scheduler);
        var device = scheduler.blocks[ID_DEVICE_A];
        var handler = scheduler.blocks[ID_HANDLER_A];
        var worker = scheduler.blocks[ID_WORKER];
        var workerTask = worker.task;
        device.state = STATE_RUNNING;
        handler.state = STATE_RUNNING;
        worker.state = STATE_RUNNING;
        scheduler.list = device;

        // Replacing the value retains WorkerTask's exact shape and prototype, so the base graph
        // remains valid. Only CORE's all-six outer-Scheduler identity contract must decline.
        var keys = Object.keys(workerTask).join('|');
        var foreign = new Scheduler();
        var gets = 0, calls = 0, callThis = null, sourceOrder = false;
        Object.defineProperty(foreign, 'suspendCurrent', {
          configurable: true,
          get: function() {
            gets++;
            sourceOrder =
                device.state === STATE_SUSPENDED &&
                handler.state === STATE_SUSPENDED &&
                worker.state === STATE_RUNNING &&
                scheduler.currentId === ID_WORKER &&
                scheduler.currentTcb === worker;
            return function() {
              calls++;
              callThis = this;
              return null;
            };
          }
        });
        workerTask.scheduler = foreign;
        check(Object.keys(workerTask).join('|') === keys &&
              Object.getPrototypeOf(workerTask) === WorkerTask.prototype,
              'same-shape task fixture');

        scheduler.schedule();

        check(gets === 1 && calls === 1 && callThis === foreign,
              'foreign accessor and call once');
        check(sourceOrder, 'foreign accessor source position');
        check(device.state === STATE_SUSPENDED &&
              handler.state === STATE_SUSPENDED,
              'base graph continued before generic fallback');
        check(worker.state === STATE_RUNNING &&
              scheduler.currentTcb === null &&
              scheduler.currentId === ID_WORKER,
              'foreign result preserved');
        check(worker.task === workerTask && workerTask.scheduler === foreign &&
              scheduler.blocks[ID_WORKER] === worker,
              'foreign and graph owners retained');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[test]
fn jit_scheduler_graph_core_suspend_enabled_disabled_parity() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    for disabled in [false, true] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::jit_scheduler_graph_core_suspend_parity_case")
            .arg("--nocapture")
            .env("LUMEN_JIT_REGIONLOG", "1")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_CORE")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_METHOD_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_ROLE_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_ROLE_DISPATCH")
            .env_remove("LUMEN_JIT_NO_SCHED_FAST_LOOP")
            .env_remove("LUMEN_JIT_NO_SCHED_REGION");
        if disabled {
            command.env("LUMEN_JIT_NO_SCHED_GRAPH_CORE", "1");
        }

        let output = command.output().expect("run graph CORE parity child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && stdout.contains("running 1 test")
                && stderr.contains("graph_epoch=true")
                && stderr.contains(&format!("graph_core={}", !disabled)),
            "graph CORE parity child disabled={disabled} failed\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn jit_scheduler_graph_core_epoch_rebuilds_task_identities_across_calls() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph core identity refill: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
        }
        function newTask(scheduler, id, n) {
          if (id === ID_WORKER)
            return new WorkerTask(scheduler, ID_HANDLER_A, n + 11);
          if (id === ID_HANDLER_A)
            return new HandlerTask(scheduler);
          return new DeviceTask(scheduler);
        }

        var scheduler = new Scheduler();
        addSix(scheduler);
        var roles = [ID_WORKER, ID_HANDLER_A, ID_DEVICE_A];

        // Every call ends the old bounded session. Same-layout replacement records/tasks must be
        // rediscovered on the next call; stale raw frame identities must neither retain nor touch
        // the detached objects.
        for (var n = 0; n < 48; n++) {
          for (var j = 0; j < NUMBER_OF_IDS; j++)
            scheduler.blocks[j].state = STATE_HELD;

          var id = roles[n % roles.length];
          var old = scheduler.blocks[id];
          var oldTask = old.task;
          var predecessor = scheduler.blocks[id + 1];
          var task = newTask(scheduler, id, n);
          var fresh = new TaskControlBlock(
              old.link, id, old.priority, null, task);
          fresh.state = STATE_RUNNING;

          check(Object.keys(fresh).join('|') === Object.keys(old).join('|') &&
                Object.getPrototypeOf(fresh) === Object.getPrototypeOf(old),
                'same-layout TCB ' + n);
          check(Object.keys(task).join('|') === Object.keys(oldTask).join('|') &&
                Object.getPrototypeOf(task) === Object.getPrototypeOf(oldTask),
                'same-layout task ' + n);

          predecessor.link = fresh;
          scheduler.blocks[id] = fresh;
          scheduler.list = fresh;
          scheduler.schedule();

          check(fresh.state === STATE_SUSPENDED &&
                scheduler.currentId === id && scheduler.currentTcb === null,
                'fresh record result ' + n);
          check(scheduler.blocks[id] === fresh && predecessor.link === fresh &&
                fresh.task === task && task.scheduler === scheduler,
                'fresh identities published ' + n);
          check(old.state === STATE_HELD && old.task === oldTask &&
                oldTask.scheduler === scheduler,
                'detached identities untouched ' + n);
        }
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_graph_core_incoming_suspend_parity_case() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph core incoming: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++)
            scheduler.blocks[id].state = STATE_HELD;
        }

        // Three incoming DEVICE packets retain one complete canonical graph session while the
        // Active successor alternates pending RUNNABLE and final RUNNING state.
        var deviceScheduler = new Scheduler();
        addSix(deviceScheduler);
        var deviceCurrent = deviceScheduler.blocks[ID_HANDLER_A];
        var deviceTask = deviceCurrent.task;
        var d3 = new Packet(null, ID_WORKER, KIND_DEVICE);
        var d2 = new Packet(d3, ID_WORKER, KIND_DEVICE);
        var d1 = new Packet(d2, ID_WORKER, KIND_DEVICE);
        deviceCurrent.queue = d1;
        deviceCurrent.state = STATE_SUSPENDED_RUNNABLE;
        deviceScheduler.list = deviceCurrent;
        deviceScheduler.schedule();
        check(deviceTask.v2 === d1 && d1.link === d2 && d2.link === d3 &&
              d3.link === null, 'DEVICE bounded list and owners');
        check(deviceCurrent.queue === null &&
              deviceCurrent.state === STATE_SUSPENDED &&
              deviceScheduler.currentId === ID_HANDLER_A &&
              deviceScheduler.currentTcb === null,
              'DEVICE final scheduler state');

        // The second WORK packet takes the one-old-node append arm before the same CORE-backed
        // suspend tail. All other graph records remain canonical and held.
        var workScheduler = new Scheduler();
        addSix(workScheduler);
        var workCurrent = workScheduler.blocks[ID_HANDLER_A];
        var workTask = workCurrent.task;
        var w2 = new Packet(null, ID_HANDLER_A, KIND_WORK);
        var w1 = new Packet(w2, ID_HANDLER_A, KIND_WORK);
        workCurrent.queue = w1;
        workCurrent.state = STATE_SUSPENDED_RUNNABLE;
        workScheduler.list = workCurrent;
        workScheduler.schedule();
        check(workTask.v1 === w1 && w1.link === w2 && w2.link === null,
              'WORK bounded list and owners');
        check(workTask.v2 === null && workCurrent.queue === null &&
              workCurrent.state === STATE_SUSPENDED &&
              workScheduler.currentId === ID_HANDLER_A &&
              workScheduler.currentTcb === null,
              'WORK final scheduler state');
        check(deviceTask.scheduler === deviceScheduler &&
              workTask.scheduler === workScheduler &&
              deviceCurrent.link === deviceScheduler.blocks[ID_WORKER] &&
              workCurrent.link === workScheduler.blocks[ID_WORKER],
              'canonical graph identities retained');

        [deviceCurrent.state, deviceCurrent.queue === null,
         deviceTask.v2 === d1 && d3.link === null,
         deviceScheduler.currentTcb === null,
         workCurrent.state, workCurrent.queue === null,
         workTask.v1 === w1 && w2.link === null,
         workScheduler.currentTcb === null].join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "2|true|true|true|2|true|true|true");
}

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[test]
fn jit_scheduler_graph_core_incoming_suspend_enabled_disabled_parity() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    for disabled in [false, true] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::jit_scheduler_graph_core_incoming_suspend_parity_case")
            .arg("--nocapture")
            .env("LUMEN_JIT_REGIONLOG", "1")
            .env_remove("LUMEN_JIT_SCHED_TRACE")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_CORE_INCOMING")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_CORE")
            .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_METHOD_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_ROLE_EPOCH")
            .env_remove("LUMEN_JIT_NO_SCHED_ROLE_DISPATCH")
            .env_remove("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH")
            .env_remove("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING_SUSPEND")
            .env_remove("LUMEN_JIT_NO_SCHED_FAST_LOOP")
            .env_remove("LUMEN_JIT_NO_SCHED_REGION");
        if disabled {
            command.env("LUMEN_JIT_NO_SCHED_GRAPH_CORE_INCOMING", "1");
        }

        let output = command
            .output()
            .expect("run graph CORE incoming parity child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && stdout.contains("running 1 test")
                && stderr.contains("graph_epoch=true")
                && stderr.contains("graph_core=true")
                && stderr.contains(&format!("graph_core_incoming={}", !disabled)),
            "graph CORE incoming parity child disabled={disabled} failed\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[test]
fn jit_scheduler_graph_core_incoming_suspend_uses_saved_record_under_trace() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    let output = Command::new(&executable)
        .arg("--exact")
        .arg("tests::jit_scheduler_graph_core_incoming_suspend_parity_case")
        .arg("--nocapture")
        .env("LUMEN_JIT_REGIONLOG", "1")
        .env("LUMEN_JIT_SCHED_TRACE", "1")
        .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_CORE_INCOMING")
        .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_CORE")
        .env_remove("LUMEN_JIT_NO_SCHED_GRAPH_EPOCH")
        .env_remove("LUMEN_JIT_NO_SCHED_METHOD_EPOCH")
        .env_remove("LUMEN_JIT_NO_SCHED_ROLE_EPOCH")
        .env_remove("LUMEN_JIT_NO_SCHED_ROLE_DISPATCH")
        .env_remove("LUMEN_JIT_NO_SCHED_ACTIVE_PACKET_ROLE_DISPATCH")
        .env_remove("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING_SUSPEND")
        .env_remove("LUMEN_JIT_NO_SCHED_FAST_LOOP")
        .env_remove("LUMEN_JIT_NO_SCHED_REGION")
        .output()
        .expect("run graph CORE incoming trace child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stdout.contains("running 1 test")
            && stderr.contains("graph_epoch=true")
            && stderr.contains("graph_core=true")
            && stderr.contains("graph_core_incoming=true"),
        "graph CORE incoming trace child failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn jit_scheduler_graph_core_incoming_soft_rejects_foreign_handler_scheduler_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'graph core incoming soft reject: ' + message;
        }
        function addSix(scheduler) {
          scheduler.addIdleTask(ID_IDLE, 0, null, 1);
          scheduler.addWorkerTask(ID_WORKER, 1000, null);
          scheduler.addHandlerTask(ID_HANDLER_A, 2000, null);
          scheduler.addHandlerTask(ID_HANDLER_B, 3000, null);
          scheduler.addDeviceTask(ID_DEVICE_A, 4000, null);
          scheduler.addDeviceTask(ID_DEVICE_B, 5000, null);
          for (var id = 0; id < NUMBER_OF_IDS; id++)
            scheduler.blocks[id].state = STATE_HELD;
        }

        var scheduler = new Scheduler();
        addSix(scheduler);
        var current = scheduler.blocks[ID_HANDLER_A];
        var handler = current.task;
        var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
        current.queue = packet;
        current.state = STATE_SUSPENDED_RUNNABLE;
        scheduler.list = current;

        // A value-only replacement keeps HandlerTask's graph-proven shape/prototype. CORE must
        // remain a soft miss and generic replay must expose all prior Active/addTo effects once.
        var keys = Object.keys(handler).join('|');
        var foreign = new Scheduler();
        var gets = 0, calls = 0, callThis = null, sourceOrder = false;
        Object.defineProperty(foreign, 'suspendCurrent', {
          configurable: true,
          get: function() {
            gets++;
            sourceOrder = handler.v2 === packet && packet.link === null &&
                current.queue === null && current.state === STATE_RUNNING &&
                scheduler.currentId === ID_HANDLER_A &&
                scheduler.currentTcb === current;
            return function() {
              calls++;
              callThis = this;
              return null;
            };
          }
        });
        handler.scheduler = foreign;
        check(Object.keys(handler).join('|') === keys &&
              Object.getPrototypeOf(handler) === HandlerTask.prototype,
              'same-shape Handler fixture');

        scheduler.schedule();

        check(gets === 1 && calls === 1 && callThis === foreign,
              'foreign accessor and call once');
        check(sourceOrder, 'foreign accessor source position');
        check(handler.v2 === packet && packet.link === null &&
              current.queue === null && current.state === STATE_RUNNING,
              'generic Handler effects preserved');
        check(scheduler.currentId === ID_HANDLER_A &&
              scheduler.currentTcb === null &&
              handler.scheduler === foreign &&
              scheduler.blocks[ID_HANDLER_A] === current,
              'scheduler and graph owners retained');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_idle_stitches_releases_and_replays_late_method_guard() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        // Populate all four task-run call-cache ways and warm IdleTask's second-stage body before
        // compiling the scheduler. The cases below then enter Idle through SchedulerActive with
        // an exact Null packet, rather than calling IdleTask.run directly.
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active idle stitch: ' + message;
        }
        function oneScheduledIdle(v1, count, id) {
          var scheduler = new Scheduler();
          var idleTask = new IdleTask(scheduler, v1, count);
          var idle = new TaskControlBlock(null, ID_IDLE, 1, null, idleTask);
          idle.state = STATE_RUNNING;

          // release() turns HELD into RUNNING. The higher-priority Device then proves that the
          // returned target was published to Scheduler.currentTcb: it runs once and suspends.
          var target = new TaskControlBlock(
              null, id, 3, null, new DeviceTask(scheduler));
          target.state = STATE_HELD;
          scheduler.blocks[id] = target;
          scheduler.list = idle;
          return {
            scheduler: scheduler,
            idleTask: idleTask,
            idle: idle,
            target: target
          };
        }

        var even = oneScheduledIdle(2, 2, ID_DEVICE_A);
        even.scheduler.schedule();
        check(even.idleTask.count === 1 && even.idleTask.v1 === 1,
              'even release numerics');
        check(even.target.state === STATE_SUSPENDED,
              'even target became current and ran');
        check(even.scheduler.currentId === ID_DEVICE_A &&
              even.scheduler.currentTcb === null && even.scheduler.holdCount === 0,
              'even scheduler continuation');

        var odd = oneScheduledIdle(3, 2, ID_DEVICE_B);
        odd.scheduler.schedule();
        check(odd.idleTask.count === 1 &&
              odd.idleTask.v1 === ((3 >> 1) ^ 0xD008),
              'odd release numerics');
        check(odd.target.state === STATE_SUSPENDED,
              'odd target became current and ran');
        check(odd.scheduler.currentId === ID_DEVICE_B &&
              odd.scheduler.currentTcb === null && odd.scheduler.holdCount === 0,
              'odd scheduler continuation');

        // count==1 cannot use the release transaction: ordinary IdleTask.run decrements to zero,
        // calls holdCurrent, and leaves v1 untouched.
        var finalCase = oneScheduledIdle(9, 1, ID_DEVICE_A);
        finalCase.scheduler.schedule();
        check(finalCase.idleTask.count === 0 && finalCase.idleTask.v1 === 9,
              'final iteration numerics');
        check(finalCase.idle.state === STATE_HELD &&
              finalCase.scheduler.holdCount === 1 &&
              finalCase.target.state === STATE_HELD &&
              finalCase.scheduler.currentTcb === null,
              'final iteration replayed hold');

        // This identity guard is deliberately late in the fused transaction. It must decline
        // before any write; baseline replay then performs count, v1, and mark exactly once.
        var originalMark = TaskControlBlock.prototype.markAsNotHeld;
        var markCalls = 0;
        TaskControlBlock.prototype.markAsNotHeld = function() {
          markCalls++;
          return originalMark.call(this);
        };
        var changedMark = oneScheduledIdle(2, 2, ID_DEVICE_A);
        changedMark.scheduler.schedule();
        TaskControlBlock.prototype.markAsNotHeld = originalMark;
        check(markCalls === 1, 'changed mark method called once');
        check(changedMark.idleTask.count === 1 && changedMark.idleTask.v1 === 1,
              'changed mark replayed Idle writes once');
        check(changedMark.target.state === STATE_SUSPENDED &&
              changedMark.scheduler.currentTcb === null,
              'changed mark replay preserved scheduler result');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_idle_replays_changed_callees_and_alias_edges_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        // Seed every Scheduler task-run way and compile the Idle child transaction. Each case
        // below reaches Idle through the scheduler's exact Null-packet active dispatch.
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active idle replay edges: ' + message;
        }
        function oneScheduledIdle(v1, count, id) {
          var scheduler = new Scheduler();
          var idleTask = new IdleTask(scheduler, v1, count);
          var idle = new TaskControlBlock(null, ID_IDLE, 1, null, idleTask);
          idle.state = STATE_RUNNING;
          var target = new TaskControlBlock(
              null, id, 3, null, new DeviceTask(scheduler));
          target.state = STATE_HELD;
          scheduler.blocks[id] = target;
          scheduler.list = idle;
          return {
            scheduler: scheduler,
            idleTask: idleTask,
            idle: idle,
            target: target
          };
        }
        function checkReleased(one, count, v1, label) {
          check(one.idleTask.count === count && one.idleTask.v1 === v1,
                label + ' Idle writes');
          check(one.target.state === STATE_SUSPENDED,
                label + ' target ran once');
          check(one.scheduler.currentId === one.target.id &&
                one.scheduler.currentTcb === null &&
                one.scheduler.holdCount === 0,
                label + ' scheduler completed');
        }

        // Changing IdleTask.run must decline before count/v1 are touched. Ordinary replay then
        // enters the replacement exactly once with the original values.
        var originalIdleRun = IdleTask.prototype.run;
        var runHits = 0, runCount = -1, runV1 = -1, runSawNull = false;
        IdleTask.prototype.run = function(packet) {
          runHits++;
          runCount = this.count;
          runV1 = this.v1;
          runSawNull = packet === null;
          return originalIdleRun.call(this, packet);
        };
        var changedRun = oneScheduledIdle(2, 2, ID_DEVICE_A);
        changedRun.scheduler.schedule();
        IdleTask.prototype.run = originalIdleRun;
        check(runHits === 1 && runCount === 2 && runV1 === 2 && runSawNull,
              'changed run entered once before writes');
        checkReleased(changedRun, 1, 1, 'changed run');

        // release is called after the source-level count/v1 updates. Its identity guard still
        // has to decline before the fused transaction commits, so replay must expose exactly one
        // already-updated call to the replacement.
        var originalRelease = Scheduler.prototype.release;
        var releaseHits = 0, releaseCount = -1, releaseV1 = -1;
        Scheduler.prototype.release = function(id) {
          releaseHits++;
          releaseCount = this.currentTcb.task.count;
          releaseV1 = this.currentTcb.task.v1;
          return originalRelease.call(this, id);
        };
        var changedRelease = oneScheduledIdle(3, 2, ID_DEVICE_B);
        changedRelease.scheduler.schedule();
        Scheduler.prototype.release = originalRelease;
        check(releaseHits === 1 && releaseCount === 1 &&
              releaseV1 === ((3 >> 1) ^ 0xD008),
              'changed release observed one source-ordered update');
        checkReleased(changedRelease, 1, ((3 >> 1) ^ 0xD008),
                      'changed release');

        // An IdleTask may point at a different, shape-compatible Scheduler. The stitched path
        // cannot substitute the outer scheduler; replay must mutate the foreign block once while
        // the outer loop continues with the returned TCB.
        var wrongScheduler = oneScheduledIdle(2, 2, ID_DEVICE_A);
        var foreign = new Scheduler();
        foreign.currentTcb = wrongScheduler.idle;
        foreign.blocks[ID_DEVICE_A] = wrongScheduler.target;
        wrongScheduler.idleTask.scheduler = foreign;
        wrongScheduler.scheduler.schedule();
        checkReleased(wrongScheduler, 1, 1, 'foreign scheduler');
        check(foreign.currentTcb === wrongScheduler.idle &&
              foreign.holdCount === 0 && foreign.queueCount === 0,
              'foreign scheduler identity preserved');

        // A non-writable currentTcb entry must force replay. The first assignment intentionally
        // fails and schedules Idle a second time. markAsHeld restores writability before the
        // count==0 fallback returns, making the exact two-iteration result observable and finite.
        var nonWritable = oneScheduledIdle(2, 2, ID_DEVICE_A);
        var originalMarkHeld = TaskControlBlock.prototype.markAsHeld;
        var heldHits = 0;
        TaskControlBlock.prototype.markAsHeld = function() {
          if (this === nonWritable.idle) {
            heldHits++;
            Object.defineProperty(nonWritable.scheduler, 'currentTcb', {
              value: nonWritable.scheduler.currentTcb,
              writable: true,
              configurable: true
            });
          }
          return originalMarkHeld.call(this);
        };
        Object.defineProperty(nonWritable.scheduler, 'currentTcb', {
          value: nonWritable.idle,
          writable: false,
          configurable: true
        });
        nonWritable.scheduler.schedule();
        TaskControlBlock.prototype.markAsHeld = originalMarkHeld;
        check(heldHits === 1 && nonWritable.idleTask.count === 0 &&
              nonWritable.idleTask.v1 === 1,
              'non-writable current replayed exactly twice');
        check(nonWritable.idle.state === STATE_HELD &&
              nonWritable.target.state === STATE_RUNNING &&
              nonWritable.scheduler.holdCount === 1 &&
              nonWritable.scheduler.currentId === ID_IDLE &&
              nonWritable.scheduler.currentTcb === null,
              'non-writable current preserved assignment semantics');

        // release(target === current) is legal and returns current. It is not an ownership
        // transfer, so the fused higher-priority path must replay; Idle then reaches hold once.
        var selfTarget = oneScheduledIdle(2, 2, ID_DEVICE_A);
        selfTarget.scheduler.blocks[ID_DEVICE_A] = selfTarget.idle;
        selfTarget.scheduler.schedule();
        check(selfTarget.idleTask.count === 0 && selfTarget.idleTask.v1 === 1,
              'self target Idle writes once per source iteration');
        check(selfTarget.idle.state === STATE_HELD &&
              selfTarget.target.state === STATE_HELD &&
              selfTarget.scheduler.holdCount === 1 &&
              selfTarget.scheduler.currentId === ID_IDLE &&
              selfTarget.scheduler.currentTcb === null,
              'self target replay completed without owner transfer');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_worker_null_stitches_suspend_and_replays_method_guards() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        // Fill Scheduler's polymorphic task-run profile and trigger its second-stage compile.
        // The custom cases below then reach WorkerTask through SchedulerActive with an exact
        // Null packet, which is the pure bridge to suspendCurrent covered by this regression.
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active worker stitch: ' + message;
        }
        function oneScheduledWorker() {
          var scheduler = new Scheduler();
          var task = new WorkerTask(scheduler, ID_HANDLER_A, 0);
          var worker = new TaskControlBlock(null, ID_WORKER, 1000, null, task);
          worker.state = STATE_RUNNING;
          scheduler.blocks[ID_WORKER] = worker;
          scheduler.list = worker;
          return { scheduler: scheduler, task: task, worker: worker };
        }
        function checkCompleted(one, label) {
          check(one.worker.state === STATE_SUSPENDED, label + ' suspended');
          check(one.scheduler.currentId === ID_WORKER, label + ' current id');
          check(one.scheduler.currentTcb === null, label + ' completed');
        }

        var direct = oneScheduledWorker();
        direct.scheduler.schedule();
        checkCompleted(direct, 'direct');

        // A changed WorkerTask.run must decline the bridge before touching the TCB. The ordinary
        // call observes STATE_RUNNING and executes exactly once.
        var originalRun = WorkerTask.prototype.run;
        var runHits = 0, runEntryState = -1, runSawNull = false;
        WorkerTask.prototype.run = function(packet) {
          runHits++;
          runEntryState = this.scheduler.currentTcb.state;
          runSawNull = packet === null;
          return originalRun.call(this, packet);
        };
        var changedRun = oneScheduledWorker();
        changedRun.scheduler.schedule();
        WorkerTask.prototype.run = originalRun;
        check(runHits === 1 && runEntryState === STATE_RUNNING && runSawNull,
              'changed run replayed once before effects');
        checkCompleted(changedRun, 'changed run');

        // suspendCurrent is guarded before the shared state transaction. Its replacement must
        // likewise see the untouched state and run once through the canonical call path.
        var originalSuspend = Scheduler.prototype.suspendCurrent;
        var suspendHits = 0, suspendEntryState = -1;
        Scheduler.prototype.suspendCurrent = function() {
          suspendHits++;
          suspendEntryState = this.currentTcb.state;
          return originalSuspend.call(this);
        };
        var changedSuspend = oneScheduledWorker();
        changedSuspend.scheduler.schedule();
        Scheduler.prototype.suspendCurrent = originalSuspend;
        check(suspendHits === 1 && suspendEntryState === STATE_RUNNING,
              'changed suspend replayed once before effects');
        checkCompleted(changedSuspend, 'changed suspend');

        // The nested method guard is deliberately late, but still precedes the state write.
        // Observing STATE_RUNNING here rejects a partial fast update followed by generic replay.
        var originalMark = TaskControlBlock.prototype.markAsSuspended;
        var markHits = 0, markEntryState = -1;
        TaskControlBlock.prototype.markAsSuspended = function() {
          markHits++;
          markEntryState = this.state;
          return originalMark.call(this);
        };
        var changedMark = oneScheduledWorker();
        changedMark.scheduler.schedule();
        TaskControlBlock.prototype.markAsSuspended = originalMark;
        check(markHits === 1 && markEntryState === STATE_RUNNING,
              'changed mark replayed once before effects');
        checkCompleted(changedMark, 'changed mark');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_worker_packet_preempts_and_preserves_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active worker packet owners: ' + message;
        }
        function oneWorker(link, v1, v2) {
          var scheduler = new Scheduler();
          var task = new WorkerTask(scheduler, v1, v2);
          var packet = new Packet(link, ID_WORKER, KIND_WORK);
          var worker = new TaskControlBlock(
              null, ID_WORKER, 1000, packet, task);
          var target = new TaskControlBlock(
              null, ID_HANDLER_B, 2000, null, new HandlerTask(scheduler));
          // queue() still publishes and preempts to this TCB, but the held bit keeps the
          // following scheduler iteration finite and leaves the packet available to inspect.
          target.state = STATE_SUSPENDED | STATE_HELD;
          for (var id = 0; id < NUMBER_OF_IDS; id++) scheduler.blocks[id] = target;
          scheduler.blocks[ID_WORKER] = worker;
          scheduler.list = worker;
          return {
            scheduler: scheduler,
            task: task,
            worker: worker,
            target: target,
            packet: packet
          };
        }
        function checkQueued(one, sourceQueue, sourceState, targetState, label) {
          check(one.scheduler.queueCount === 1, label + ' queue count');
          check(one.worker.queue === sourceQueue && one.worker.state === sourceState,
                label + ' source dequeue');
          check(one.target.queue === one.packet && one.target.state === targetState,
                label + ' target publication');
          check(one.packet.link === null && one.packet.id === ID_WORKER &&
                one.packet.a1 === 0, label + ' queue prefix');
          check(one.scheduler.currentId === ID_WORKER &&
                one.scheduler.currentTcb === null, label + ' scheduler completion');
        }

        // Exercise the canonical toggle and the v2 wrap while retaining the packet payload and
        // the source successor through independent owners.
        var successor = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        successor.a1 = 77;
        var direct = oneWorker(successor, ID_HANDLER_A, 24);
        var payload = direct.packet.a2;
        direct.scheduler.schedule();
        check(direct.task.v1 === ID_HANDLER_B && direct.task.v2 === 2,
              'direct Worker numerics');
        check(payload === direct.packet.a2 && payload.join(',') === '25,26,1,2',
              'direct payload identity');
        check(successor.a1 === 77, 'direct successor remains live');
        checkQueued(direct, successor, STATE_RUNNABLE,
                    STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE, 'direct');

        var reverse = oneWorker(null, ID_HANDLER_B, 26);
        reverse.scheduler.schedule();
        check(reverse.task.v1 === ID_HANDLER_A && reverse.task.v2 === 4,
              'reverse Worker numerics');
        check(reverse.packet.a2.join(',') === '1,2,3,4', 'reverse payload');
        checkQueued(reverse, null, STATE_RUNNING,
                    STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE, 'reverse');

        // The source successor can be the packet itself. queue() later clears packet.link, but
        // both TCB queues must retain their separate owners of the packet.
        var selfLink = oneWorker(null, ID_HANDLER_A, 0);
        selfLink.packet.link = selfLink.packet;
        selfLink.scheduler.schedule();
        check(selfLink.worker.queue === selfLink.packet &&
              selfLink.target.queue === selfLink.packet,
              'self link aliases both queues');
        check(selfLink.packet.link === null && selfLink.task.v2 === 4,
              'self link queue rewrite');
        checkQueued(selfLink, selfLink.packet, STATE_RUNNABLE,
                    STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE, 'self link');

        // Likewise, moving the current TCB itself out of packet.link must not release it before
        // the source queue takes ownership. Break the intentional cycle after observing it.
        var currentLink = oneWorker(null, ID_HANDLER_A, 0);
        currentLink.packet.link = currentLink.worker;
        currentLink.scheduler.schedule();
        check(currentLink.worker.queue === currentLink.worker &&
              currentLink.worker.priority === 1000, 'current link survives transfer');
        checkQueued(currentLink, currentLink.worker, STATE_RUNNABLE,
                    STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE, 'current link');
        currentLink.worker.queue = null;

        // Worker names are live values. These cases must replay when they no longer match the
        // profiled Richards constants rather than baking old loop bounds, ids, or state bits.
        var oldDataSize = DATA_SIZE;
        var shortLoop = oneWorker(null, ID_HANDLER_A, 0);
        DATA_SIZE = 2;
        shortLoop.scheduler.schedule();
        DATA_SIZE = oldDataSize;
        check(shortLoop.task.v2 === 2 && shortLoop.packet.a2[0] === 1 &&
              shortLoop.packet.a2[1] === 2 && !Object.hasOwn(shortLoop.packet.a2, 2) &&
              !Object.hasOwn(shortLoop.packet.a2, 3), 'live DATA_SIZE');
        checkQueued(shortLoop, null, STATE_RUNNING,
                    STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE, 'short loop');

        var oldSuspendedRunnable = STATE_SUSPENDED_RUNNABLE;
        var oldRunning = STATE_RUNNING;
        var oldRunnable = STATE_RUNNABLE;
        var liveState = oneWorker(null, ID_HANDLER_A, 0);
        STATE_SUSPENDED_RUNNABLE = 8;
        STATE_RUNNING = 16;
        STATE_RUNNABLE = 17;
        liveState.worker.state = STATE_SUSPENDED_RUNNABLE;
        liveState.scheduler.schedule();
        STATE_SUSPENDED_RUNNABLE = oldSuspendedRunnable;
        STATE_RUNNING = oldRunning;
        STATE_RUNNABLE = oldRunnable;
        check(liveState.worker.state === 16 && liveState.target.state === 23,
              'live active state names');
        check(liveState.scheduler.queueCount === 1 &&
              liveState.scheduler.currentTcb === null, 'live state completion');

        var oldHandlerA = ID_HANDLER_A;
        var oldHandlerB = ID_HANDLER_B;
        var liveIds = oneWorker(null, ID_HANDLER_A, 0);
        ID_HANDLER_A = 7;
        ID_HANDLER_B = 8;
        liveIds.task.v1 = ID_HANDLER_A;
        liveIds.scheduler.blocks[ID_HANDLER_B] = liveIds.target;
        liveIds.scheduler.schedule();
        ID_HANDLER_A = oldHandlerA;
        ID_HANDLER_B = oldHandlerB;
        check(liveIds.task.v1 === 8 && liveIds.packet.id === ID_WORKER,
              'live handler ids');
        check(liveIds.target.queue === liveIds.packet &&
              liveIds.scheduler.currentTcb === null, 'live id target');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_worker_packet_replays_changed_methods_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active worker packet methods: ' + message;
        }
        function oneWorker() {
          var scheduler = new Scheduler();
          var task = new WorkerTask(scheduler, ID_HANDLER_A, 24);
          var packet = new Packet(null, ID_WORKER, KIND_WORK);
          var worker = new TaskControlBlock(
              null, ID_WORKER, 1000, packet, task);
          var target = new TaskControlBlock(
              null, ID_HANDLER_B, 2000, null, new HandlerTask(scheduler));
          target.state = STATE_SUSPENDED | STATE_HELD;
          for (var id = 0; id < NUMBER_OF_IDS; id++) scheduler.blocks[id] = target;
          scheduler.blocks[ID_WORKER] = worker;
          scheduler.list = worker;
          return {
            scheduler: scheduler,
            task: task,
            worker: worker,
            target: target,
            packet: packet
          };
        }
        function checkComplete(one, targetState, label) {
          check(one.task.v1 === ID_HANDLER_B && one.task.v2 === 2,
                label + ' Worker numerics once');
          check(one.packet.a2.join(',') === '25,26,1,2',
                label + ' payload once');
          check(one.worker.queue === null && one.worker.state === STATE_RUNNING,
                label + ' source dequeue');
          check(one.scheduler.queueCount === 1 && one.target.queue === one.packet &&
                one.target.state === targetState, label + ' target publish');
          check(one.packet.link === null && one.packet.id === ID_WORKER &&
                one.scheduler.currentTcb === null, label + ' completion');
        }

        // TCB.run dequeues and updates state before looking up WorkerTask.run. A replacement must
        // therefore enter once after those effects, but before any Worker field or packet write.
        var originalWorkerRun = WorkerTask.prototype.run;
        var runHits = 0, runPacket = null, runState = -1, runQueue = 1;
        var runV1 = -1, runV2 = -1;
        WorkerTask.prototype.run = function(packet) {
          runHits++;
          runPacket = packet;
          runState = this.scheduler.currentTcb.state;
          runQueue = this.scheduler.currentTcb.queue;
          runV1 = this.v1;
          runV2 = this.v2;
          return originalWorkerRun.call(this, packet);
        };
        var changedRun = oneWorker();
        changedRun.scheduler.schedule();
        WorkerTask.prototype.run = originalWorkerRun;
        check(runHits === 1 && runPacket === changedRun.packet &&
              runState === STATE_RUNNING && runQueue === null &&
              runV1 === ID_HANDLER_A && runV2 === 24,
              'changed run source-order entry');
        checkComplete(changedRun,
                      STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE,
                      'changed run');

        // queue is looked up after every Worker mutation. Its replacement also changes a state
        // name before delegating, so checkPriorityAdd must consume that new value in this call.
        var originalQueue = Scheduler.prototype.queue;
        var queueHits = 0, queueSawCurrent = false, queueCountAtEntry = -1;
        var queueV1 = -1, queueV2 = -1, queueId = -1, queuePayload = '';
        var oldRunnable = STATE_RUNNABLE;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          queueSawCurrent = this.currentTcb === changedQueue.worker;
          queueCountAtEntry = this.queueCount;
          queueV1 = changedQueue.task.v1;
          queueV2 = changedQueue.task.v2;
          queueId = packet.id;
          queuePayload = packet.a2.join(',');
          STATE_RUNNABLE = 8;
          return originalQueue.call(this, packet);
        };
        var changedQueue = oneWorker();
        changedQueue.scheduler.schedule();
        Scheduler.prototype.queue = originalQueue;
        STATE_RUNNABLE = oldRunnable;
        check(queueHits === 1 && queueSawCurrent && queueCountAtEntry === 0 &&
              queueV1 === ID_HANDLER_B && queueV2 === 2 &&
              queueId === ID_HANDLER_B && queuePayload === '25,26,1,2',
              'changed queue sees Worker effects once');
        checkComplete(changedQueue, STATE_SUSPENDED | STATE_HELD | 8,
                      'changed queue');

        // checkPriorityAdd is looked up only after Scheduler.queue's prefix has committed, while
        // the target queue and state are still untouched.
        var originalCheck = TaskControlBlock.prototype.checkPriorityAdd;
        var checkHits = 0, checkCount = -1, checkPacketId = -1;
        var checkTargetQueue = 1, checkTargetState = -1, checkTask = null;
        TaskControlBlock.prototype.checkPriorityAdd = function(task, packet) {
          checkHits++;
          checkCount = changedCheck.scheduler.queueCount;
          checkPacketId = packet.id;
          checkTargetQueue = this.queue;
          checkTargetState = this.state;
          checkTask = task;
          return originalCheck.call(this, task, packet);
        };
        var changedCheck = oneWorker();
        changedCheck.scheduler.schedule();
        TaskControlBlock.prototype.checkPriorityAdd = originalCheck;
        check(checkHits === 1 && checkCount === 1 &&
              checkPacketId === ID_WORKER && checkTargetQueue === null &&
              checkTargetState === (STATE_SUSPENDED | STATE_HELD) &&
              checkTask === changedCheck.worker,
              'changed check sees queue prefix once');
        checkComplete(changedCheck,
                      STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE,
                      'changed check');

        // markAsRunnable is later again: target.queue has been published, but its state has not
        // yet changed. A replay after a partial native commit would make either observation fail.
        var originalMark = TaskControlBlock.prototype.markAsRunnable;
        var markHits = 0, markQueue = null, markState = -1;
        TaskControlBlock.prototype.markAsRunnable = function() {
          markHits++;
          markQueue = this.queue;
          markState = this.state;
          return originalMark.call(this);
        };
        var changedMark = oneWorker();
        changedMark.scheduler.schedule();
        TaskControlBlock.prototype.markAsRunnable = originalMark;
        check(markHits === 1 && markQueue === changedMark.packet &&
              markState === (STATE_SUSPENDED | STATE_HELD),
              'changed mark source-order entry');
        checkComplete(changedMark,
                      STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE,
                      'changed mark');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_worker_packet_preserves_source_order_on_throws() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active worker packet throws: ' + message;
        }
        function oneWorker(link, v2) {
          var scheduler = new Scheduler();
          var task = new WorkerTask(scheduler, ID_HANDLER_A, v2);
          var packet = new Packet(link, ID_WORKER, KIND_WORK);
          var worker = new TaskControlBlock(
              null, ID_WORKER, 1000, packet, task);
          var target = new TaskControlBlock(
              null, ID_HANDLER_B, 2000, null, new HandlerTask(scheduler));
          target.state = STATE_SUSPENDED | STATE_HELD;
          for (var id = 0; id < NUMBER_OF_IDS; id++) scheduler.blocks[id] = target;
          scheduler.blocks[ID_WORKER] = worker;
          scheduler.list = worker;
          return {
            scheduler: scheduler,
            task: task,
            worker: worker,
            target: target,
            packet: packet
          };
        }

        // The third dense write throws after v1/id/a1 and three v2 updates. Earlier element writes
        // and TCB.run's dequeue must survive, but Scheduler.queue must not have begun.
        var elementThrow = oneWorker(null, 0);
        var elementSets = 0;
        Object.defineProperty(elementThrow.packet.a2, '2', {
          configurable: true,
          set: function(value) { elementSets++; throw 'element boom'; }
        });
        var elementError = '';
        try { elementThrow.scheduler.schedule(); } catch (e) { elementError = e; }
        check(elementError === 'element boom' && elementSets === 1,
              'element setter throws once');
        check(elementThrow.worker.queue === null &&
              elementThrow.worker.state === STATE_RUNNING,
              'element throw source dequeue');
        check(elementThrow.task.v1 === ID_HANDLER_B && elementThrow.task.v2 === 3,
              'element throw Worker numerics');
        check(elementThrow.packet.id === ID_HANDLER_B && elementThrow.packet.a1 === 0 &&
              elementThrow.packet.a2[0] === 1 && elementThrow.packet.a2[1] === 2 &&
              Object.hasOwn(elementThrow.packet.a2, 2) &&
              !Object.hasOwn(elementThrow.packet.a2, 3),
              'element throw partial payload');
        check(elementThrow.scheduler.queueCount === 0 &&
              elementThrow.target.queue === null &&
              elementThrow.scheduler.currentTcb === elementThrow.worker,
              'element throw stops before queue');

        // scheduler is read only after the complete Worker loop. Its getter observes all Worker
        // effects and throws before queue(), while packet.link and the dequeued successor coexist.
        var successor = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        successor.a1 = 91;
        var schedulerThrow = oneWorker(successor, 0);
        var schedulerGets = 0;
        Object.defineProperty(schedulerThrow.task, 'scheduler', {
          configurable: true,
          get: function() { schedulerGets++; throw 'scheduler boom'; }
        });
        var schedulerError = '';
        try { schedulerThrow.scheduler.schedule(); } catch (e) { schedulerError = e; }
        check(schedulerError === 'scheduler boom' && schedulerGets === 1,
              'scheduler getter throws once');
        check(schedulerThrow.worker.queue === successor &&
              schedulerThrow.worker.state === STATE_RUNNABLE &&
              schedulerThrow.packet.link === successor && successor.a1 === 91,
              'scheduler throw preserves successor owners');
        check(schedulerThrow.task.v1 === ID_HANDLER_B && schedulerThrow.task.v2 === 4 &&
              schedulerThrow.packet.id === ID_HANDLER_B &&
              schedulerThrow.packet.a2.join(',') === '1,2,3,4',
              'scheduler throw preserves Worker writes');
        check(schedulerThrow.scheduler.queueCount === 0 &&
              schedulerThrow.scheduler.currentTcb === schedulerThrow.worker,
              'scheduler throw stops before queue');

        // A current-priority getter runs after queueCount, packet rewrites, target publication,
        // and markAsRunnable. Throwing here must preserve all those effects exactly once while
        // leaving Scheduler.currentTcb on the source TCB.
        var priorityThrow = oneWorker(null, 0);
        var priorityGets = 0;
        Object.defineProperty(priorityThrow.worker, 'priority', {
          configurable: true,
          get: function() { priorityGets++; throw 'priority boom'; }
        });
        var priorityError = '';
        try { priorityThrow.scheduler.schedule(); } catch (e) { priorityError = e; }
        check(priorityError === 'priority boom' && priorityGets === 1,
              'priority getter throws once');
        check(priorityThrow.task.v1 === ID_HANDLER_B && priorityThrow.task.v2 === 4 &&
              priorityThrow.worker.queue === null &&
              priorityThrow.worker.state === STATE_RUNNING,
              'priority throw Worker effects');
        check(priorityThrow.scheduler.queueCount === 1 &&
              priorityThrow.packet.link === null &&
              priorityThrow.packet.id === ID_WORKER,
              'priority throw queue prefix');
        check(priorityThrow.target.queue === priorityThrow.packet &&
              priorityThrow.target.state ===
                  (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE) &&
              priorityThrow.scheduler.currentTcb === priorityThrow.worker,
              'priority throw target effects');

        // The outer assignment is later than WorkerTask.run and Scheduler.queue. A setter that
        // rejects the preempting target must see the initial source assignment first and retain
        // the old current value after every preceding effect has committed.
        var currentThrow = oneWorker(null, 0);
        var storedCurrent = null, currentGets = 0, currentSets = 0;
        Object.defineProperty(currentThrow.scheduler, 'currentTcb', {
          configurable: true,
          get: function() { currentGets++; return storedCurrent; },
          set: function(value) {
            currentSets++;
            if (value === currentThrow.target) throw 'current boom';
            storedCurrent = value;
          }
        });
        var currentError = '';
        try { currentThrow.scheduler.schedule(); } catch (e) { currentError = e; }
        check(currentError === 'current boom' && currentSets === 2 && currentGets > 0,
              'current setter source order');
        check(storedCurrent === currentThrow.worker &&
              currentThrow.scheduler.queueCount === 1,
              'current setter retains source');
        check(currentThrow.task.v1 === ID_HANDLER_B && currentThrow.task.v2 === 4 &&
              currentThrow.target.queue === currentThrow.packet &&
              currentThrow.target.state ===
                  (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE),
              'current setter preserves prior effects');

        // An element setter is arbitrary user code. Mutating both loop and queue globals during
        // the first store must terminate the loop at one element and affect the later target mark.
        var liveSetter = oneWorker(null, 0);
        var oldDataSize = DATA_SIZE;
        var oldRunnable = STATE_RUNNABLE;
        var storedElement = -1, liveSets = 0;
        Object.defineProperty(liveSetter.packet.a2, '0', {
          configurable: true,
          get: function() { return storedElement; },
          set: function(value) {
            liveSets++;
            storedElement = value;
            DATA_SIZE = 1;
            STATE_RUNNABLE = 8;
          }
        });
        liveSetter.scheduler.schedule();
        DATA_SIZE = oldDataSize;
        STATE_RUNNABLE = oldRunnable;
        check(liveSets === 1 && storedElement === 1 && liveSetter.task.v2 === 1 &&
              !Object.hasOwn(liveSetter.packet.a2, 1),
              'live setter changes loop bound');
        check(liveSetter.scheduler.queueCount === 1 &&
              liveSetter.target.queue === liveSetter.packet &&
              liveSetter.target.state === (STATE_SUSPENDED | STATE_HELD | 8) &&
              liveSetter.scheduler.currentTcb === null,
              'live setter changes runnable bit');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_idle_release_flattens_both_branches_and_preserves_return_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'idle release owners: ' + message;
        }
        function oneIdle(v1, count, id, targetState, targetPriority, currentPriority) {
          var scheduler = new Scheduler();
          var current = new TaskControlBlock(null, ID_IDLE, currentPriority, null, {});
          current.state = STATE_RUNNING;
          var target = new TaskControlBlock(null, id, targetPriority, null, {marker: 77});
          target.state = targetState;
          scheduler.blocks[id] = target;
          scheduler.currentTcb = current;
          return {
            scheduler: scheduler,
            idle: new IdleTask(scheduler, v1, count),
            current: current,
            target: target,
            id: id
          };
        }

        var even = oneIdle(2, 2, ID_DEVICE_A,
                           STATE_HELD | STATE_RUNNABLE, 3, 2);
        var evenResult = even.idle.run(null);
        check(evenResult === even.target, 'even return identity');
        check(even.idle.count === 1 && even.idle.v1 === 1, 'even numerics');
        check(even.target.state === STATE_RUNNABLE, 'even state');
        check(even.scheduler.currentTcb === even.current, 'even current untouched');

        var odd = oneIdle(3, 2, ID_DEVICE_B,
                          STATE_HELD | STATE_SUSPENDED | STATE_RUNNABLE, 4, 2);
        var oddResult = odd.idle.run(null);
        check(oddResult === odd.target, 'odd return identity');
        check(odd.idle.count === 1 && odd.idle.v1 === ((3 >> 1) ^ 0xD008),
              'odd numerics');
        check(odd.target.state === (STATE_SUSPENDED | STATE_RUNNABLE), 'odd state');

        // Drop every source owner after return. The returned Value must retain its own Rc.
        even.scheduler.blocks[even.id] = null;
        even.target = null;
        check(evenResult.task.marker === 77 && evenResult.priority === 3,
              'returned target remains owned');

        // count==1 is the final hold and must replay the untouched ordinary function.
        var finalCase = oneIdle(9, 1, ID_DEVICE_A, STATE_HELD, 1, 2);
        var tail = new TaskControlBlock(null, ID_WORKER, 1, null, {});
        finalCase.current.link = tail;
        var finalResult = finalCase.idle.run(null);
        check(finalResult === tail && finalCase.idle.count === 0, 'final hold return');
        check(finalCase.scheduler.holdCount === 1 &&
              finalCase.current.state === STATE_HELD, 'final hold effects');
        check(finalCase.idle.v1 === 9, 'final hold leaves v1');

        // Unsupported outcomes replay from pc0 and apply their ordinary effects exactly once.
        var low = oneIdle(2, 2, ID_DEVICE_A, STATE_HELD, 1, 2);
        var lowResult = low.idle.run(null);
        check(lowResult === low.current && low.idle.count === 1 && low.idle.v1 === 1,
              'nonpreempt replay');
        check(low.target.state === STATE_RUNNING, 'nonpreempt mark');
        var missing = oneIdle(2, 2, ID_DEVICE_A, STATE_HELD, 3, 2);
        missing.scheduler.blocks[ID_DEVICE_A] = null;
        var missingResult = missing.idle.run(null);
        check(missingResult === null && missing.idle.count === 1 && missing.idle.v1 === 1,
              'missing target replay');

        // IDs and the state mask are live bindings, not constants baked into native code.
        var oldA = ID_DEVICE_A;
        ID_DEVICE_A = 1;
        var liveId = oneIdle(2, 2, ID_DEVICE_A, STATE_HELD, 3, 2);
        check(liveId.idle.run(null) === liveId.target, 'live device id');
        ID_DEVICE_A = oldA;
        var oldMask = STATE_NOT_HELD;
        STATE_NOT_HELD = ~8;
        var liveMask = oneIdle(2, 2, ID_DEVICE_A, 13, 3, 2);
        liveMask.idle.run(null);
        check(liveMask.target.state === 5, 'live not-held mask');

        var oldB = ID_DEVICE_B;
        ID_DEVICE_B = 1;
        var liveOdd = oneIdle(3, 2, ID_DEVICE_B, 13, 3, 2);
        check(liveOdd.idle.run(null) === liveOdd.target, 'live odd device id');
        check(liveOdd.idle.v1 === ((3 >> 1) ^ 0xD008) && liveOdd.target.state === 5,
              'live odd values');
        ID_DEVICE_B = oldB;
        STATE_NOT_HELD = oldMask;

        // Signed ToInt32 shifts stay on the fast path for both branches.
        var signedOdd = oneIdle(-1, 2, ID_DEVICE_B, STATE_HELD, 3, 2);
        signedOdd.idle.run(null);
        check(signedOdd.idle.v1 === ((-1 >> 1) ^ 0xD008), 'signed odd shift');
        var signedEven = oneIdle(-2147483648, 2, ID_DEVICE_A, STATE_HELD, 3, 2);
        signedEven.idle.run(null);
        check(signedEven.idle.v1 === -1073741824, 'signed even shift');

        // target===current is the equal-priority/nonpreempting ownership case and must replay.
        var alias = oneIdle(2, 2, ID_DEVICE_A, STATE_HELD, 3, 2);
        alias.current.priority = 3;
        alias.current.state = STATE_HELD;
        alias.scheduler.blocks[ID_DEVICE_A] = alias.current;
        var aliasResult = alias.idle.run(null);
        check(aliasResult === alias.current && alias.current.state === STATE_RUNNING,
              'target current alias');
        check(alias.idle.count === 1 && alias.idle.v1 === 1, 'alias effects once');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_idle_release_replays_observable_guards_and_partial_effects_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'idle release guards: ' + message;
        }
        function oneIdle(v1) {
          var scheduler = new Scheduler();
          var current = new TaskControlBlock(null, ID_IDLE, 2, null, {});
          current.state = STATE_RUNNING;
          var target = new TaskControlBlock(null, ID_DEVICE_A, 3, null, {});
          target.state = STATE_HELD;
          scheduler.blocks[ID_DEVICE_A] = target;
          scheduler.currentTcb = current;
          return [scheduler, new IdleTask(scheduler, v1, 2), current, target];
        }

        var originalRelease = Scheduler.prototype.release, releaseCalls = 0;
        Scheduler.prototype.release = function(id) {
          releaseCalls++;
          return originalRelease.call(this, id);
        };
        var releaseCase = oneIdle(2);
        check(releaseCase[1].run(null) === releaseCase[3], 'release replacement return');
        Scheduler.prototype.release = originalRelease;
        check(releaseCalls === 1 && releaseCase[1].count === 1 &&
              releaseCase[1].v1 === 1, 'release replacement once');

        var originalMark = TaskControlBlock.prototype.markAsNotHeld, markCalls = 0;
        TaskControlBlock.prototype.markAsNotHeld = function() {
          markCalls++;
          return originalMark.call(this);
        };
        var markCase = oneIdle(2);
        markCase[1].run(null);
        TaskControlBlock.prototype.markAsNotHeld = originalMark;
        check(markCalls === 1 && markCase[3].state === STATE_RUNNING,
              'mark replacement once');

        // UpdateProp reads count, writes it, then the following GetProp reads it again.
        var countCase = oneIdle(2), countValue = 2, countGets = 0, countSets = 0;
        Object.defineProperty(countCase[1], 'count', {
          get: function() { countGets++; return countValue; },
          set: function(value) { countSets++; countValue = value; }, configurable: true
        });
        countCase[1].run(null);
        check(countGets === 2 && countSets === 1 && countValue === 1,
              'count accessor order');

        // The branch test and shift are two distinct v1 reads in the source program.
        var v1Case = oneIdle(2), v1Value = 2, v1Gets = 0, v1Sets = 0;
        Object.defineProperty(v1Case[1], 'v1', {
          get: function() { v1Gets++; return v1Value; },
          set: function(value) { v1Sets++; v1Value = value; }, configurable: true
        });
        v1Case[1].run(null);
        check(v1Gets === 2 && v1Sets === 1 && v1Value === 1,
              'v1 accessor order');

        var schedulerCase = oneIdle(2), schedulerValue = schedulerCase[0], schedulerGets = 0;
        Object.defineProperty(schedulerCase[1], 'scheduler', {
          get: function() { schedulerGets++; return schedulerValue; }, configurable: true
        });
        schedulerCase[1].run(null);
        check(schedulerGets === 1, 'scheduler accessor once');

        var stateCase = oneIdle(2), stateValue = STATE_HELD;
        var stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[3], 'state', {
          get: function() { stateGets++; return stateValue; },
          set: function(value) { stateSets++; stateValue = value; }, configurable: true
        });
        stateCase[1].run(null);
        check(stateGets === 1 && stateSets === 1 && stateValue === STATE_RUNNING,
              'state accessor once');

        // A blocks getter runs after Idle's count/v1 writes but before markAsNotHeld.
        var blocksCase = oneIdle(2), blocksGets = 0, blocksError = '';
        Object.defineProperty(blocksCase[0], 'blocks', {
          get: function() { blocksGets++; throw 'blocks boom'; }, configurable: true
        });
        try { blocksCase[1].run(null); } catch (e) { blocksError = e; }
        check(blocksError === 'blocks boom' && blocksGets === 1, 'blocks throw once');
        check(blocksCase[1].count === 1 && blocksCase[1].v1 === 1 &&
              blocksCase[3].state === STATE_HELD, 'blocks throw prefix effects');

        // An indexed accessor declines the packed-element guard, then executes once normally.
        var elementCase = oneIdle(2), elementGets = 0;
        Object.defineProperty(elementCase[0].blocks, String(ID_DEVICE_A), {
          get: function() { elementGets++; return elementCase[3]; }, configurable: true
        });
        elementCase[1].run(null);
        check(elementGets === 1 && elementCase[3].state === STATE_RUNNING,
              'blocks element accessor once');

        // A late throwing priority getter observes the earlier count, v1, and state writes once.
        var priorityCase = oneIdle(2), priorityGets = 0, priorityError = '';
        Object.defineProperty(priorityCase[3], 'priority', {
          get: function() { priorityGets++; throw 'priority boom'; }, configurable: true
        });
        try { priorityCase[1].run(null); } catch (e) { priorityError = e; }
        check(priorityError === 'priority boom' && priorityGets === 1,
              'priority throw once');
        check(priorityCase[1].count === 1 && priorityCase[1].v1 === 1 &&
              priorityCase[3].state === STATE_RUNNING, 'priority throw prefix effects');

        var currentCase = oneIdle(2), currentGets = 0, currentError = '';
        Object.defineProperty(currentCase[2], 'priority', {
          get: function() { currentGets++; throw 'current priority boom'; }, configurable: true
        });
        try { currentCase[1].run(null); } catch (e) { currentError = e; }
        check(currentError === 'current priority boom' && currentGets === 1,
              'current priority throw once');
        check(currentCase[1].count === 1 && currentCase[1].v1 === 1 &&
              currentCase[3].state === STATE_RUNNING, 'current throw prefix effects');

        var currentTcbCase = oneIdle(2), currentTcbGets = 0, currentTcbError = '';
        Object.defineProperty(currentTcbCase[0], 'currentTcb', {
          get: function() { currentTcbGets++; throw 'current tcb boom'; }, configurable: true
        });
        try { currentTcbCase[1].run(null); } catch (e) { currentTcbError = e; }
        check(currentTcbError === 'current tcb boom' && currentTcbGets === 1,
              'current tcb throw once');
        check(currentTcbCase[1].count === 1 && currentTcbCase[1].v1 === 1 &&
              currentTcbCase[3].state === STATE_RUNNING, 'current tcb prefix effects');

        var nanPriority = oneIdle(2);
        nanPriority[3].priority = NaN;
        check(nanPriority[1].run(null) === nanPriority[2] &&
              nanPriority[3].state === STATE_RUNNING, 'NaN priority replay');
        var valueOfCalls = 0, objectPriority = oneIdle(2);
        objectPriority[3].priority = {
          valueOf: function() { valueOfCalls++; return 3; }
        };
        check(objectPriority[1].run(null) === objectPriority[3] && valueOfCalls === 1,
              'object priority coercion once');

        // Coercive values deliberately replay the baseline ToInt32/Number semantics.
        var fractional = oneIdle(2);
        fractional[1].count = 2.5;
        fractional[1].run(null);
        check(fractional[1].count === 1.5 && fractional[1].v1 === 1,
              'fractional count replay');
        var stringV1 = oneIdle('3');
        stringV1[0].blocks[ID_DEVICE_B] = stringV1[3];
        stringV1[1].run(null);
        check(stringV1[1].v1 === ((3 >> 1) ^ 0xD008), 'string v1 replay');
        var oldNotHeld = STATE_NOT_HELD;
        STATE_NOT_HELD = 1.5;
        var fractionalMask = oneIdle(2);
        fractionalMask[1].run(null);
        STATE_NOT_HELD = oldNotHeld;
        check(fractionalMask[3].state === STATE_RUNNING, 'fractional mask replay');

        // A throwing replacement is reached after the two Idle writes and only once on replay.
        var throwCase = oneIdle(2), throwCalls = 0, throwError = '';
        Scheduler.prototype.release = function() { throwCalls++; throw 'release boom'; };
        try { throwCase[1].run(null); } catch (e) { throwError = e; }
        Scheduler.prototype.release = originalRelease;
        check(throwError === 'release boom' && throwCalls === 1,
              'release throw once');
        check(throwCase[1].count === 1 && throwCase[1].v1 === 1 &&
              throwCase[3].state === STATE_HELD, 'release throw prefix effects');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_device_suspend_guards_methods_globals_and_descriptors() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();

        var originalRun = DeviceTask.prototype.run, runHits = 0;
        DeviceTask.prototype.run = function(packet) {
          runHits++;
          return originalRun.call(this, packet);
        };
        runRichards();
        DeviceTask.prototype.run = originalRun;

        var originalSuspend = Scheduler.prototype.suspendCurrent, suspendHits = 0;
        Scheduler.prototype.suspendCurrent = function() {
          suspendHits++;
          return originalSuspend.call(this);
        };
        runRichards();
        Scheduler.prototype.suspendCurrent = originalSuspend;

        var originalMark = TaskControlBlock.prototype.markAsSuspended, markHits = 0;
        TaskControlBlock.prototype.markAsSuspended = function() {
          markHits++;
          return originalMark.call(this);
        };
        runRichards();
        TaskControlBlock.prototype.markAsSuspended = originalMark;

        function oneDevice() {
          var scheduler = new Scheduler();
          var device = new DeviceTask(scheduler);
          var tcb = new TaskControlBlock(null, ID_DEVICE_A, 1, null, device);
          tcb.state = STATE_RUNNING;
          scheduler.list = tcb;
          return [scheduler, device, tcb];
        }

        var oldSuspended = STATE_SUSPENDED;
        STATE_SUSPENDED = 8;
        var globalCase = oneDevice();
        globalCase[0].schedule();
        var globalState = globalCase[2].state;
        STATE_SUSPENDED = oldSuspended;

        var v1Case = oneDevice(), v1 = null, v1Gets = 0, v1Sets = 0;
        Object.defineProperty(v1Case[1], 'v1', {
          get: function() { v1Gets++; return v1; },
          set: function(x) { v1Sets++; v1 = x; },
          configurable: true
        });
        v1Case[0].schedule();

        var currentCase = oneDevice(), current = currentCase[0].currentTcb;
        var currentGets = 0, currentSets = 0;
        Object.defineProperty(currentCase[0], 'currentTcb', {
          get: function() { currentGets++; return current; },
          set: function(x) { currentSets++; current = x; },
          configurable: true
        });
        currentCase[0].schedule();

        var stateCase = oneDevice(), state = STATE_RUNNING, stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[2], 'state', {
          get: function() { stateGets++; return state; },
          set: function(x) { stateSets++; state = x; },
          configurable: true
        });
        stateCase[0].schedule();

        [runHits, suspendHits, markHits, globalState,
         v1Gets, v1Sets, v1Case[2].state,
         currentGets > 0, currentSets > 0, current === null,
         stateGets, stateSets, state].join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "2777|2324|2324|8|1|0|2|true|true|true|6|1|2");
}

#[test]
fn jit_scheduler_device_hold_guards_methods_ownership_and_numeric_updates() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();

        var originalHold = Scheduler.prototype.holdCurrent, holdHits = 0;
        Scheduler.prototype.holdCurrent = function() {
          holdHits++;
          return originalHold.call(this);
        };
        runRichards();
        Scheduler.prototype.holdCurrent = originalHold;

        var originalMark = TaskControlBlock.prototype.markAsHeld, markHits = 0;
        TaskControlBlock.prototype.markAsHeld = function() {
          markHits++;
          return originalMark.call(this);
        };
        runRichards();
        TaskControlBlock.prototype.markAsHeld = originalMark;

        function oneHold(link) {
          var scheduler = new Scheduler();
          var device = new DeviceTask(scheduler);
          var packet = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          var tcb = new TaskControlBlock(link, ID_DEVICE_A, 1, packet, device);
          scheduler.list = tcb;
          return [scheduler, device, tcb, packet];
        }

        var oldHeld = STATE_HELD;
        STATE_HELD = 8;
        var globalScheduler = new Scheduler();
        var globalDevice = new DeviceTask(globalScheduler);
        var globalPacket = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        var tail = new TaskControlBlock(null, 0, 1, null, globalDevice);
        var globalTcb = new TaskControlBlock(tail, ID_DEVICE_A, 1,
                                             globalPacket, globalDevice);
        tail.state = 8;
        globalScheduler.list = globalTcb;
        globalScheduler.schedule();
        var globalResult = [globalScheduler.holdCount, globalTcb.state,
                            globalDevice.v1 === globalPacket,
                            globalScheduler.currentTcb === null, tail.state];
        STATE_HELD = oldHeld;

        var v1Case = oneHold(null), v1 = null, v1Gets = 0, v1Sets = 0;
        Object.defineProperty(v1Case[1], 'v1', {
          get: function() { v1Gets++; return v1; },
          set: function(x) { v1Sets++; v1 = x; },
          configurable: true
        });
        v1Case[0].schedule();

        var countCase = oneHold(null), count = 0, countGets = 0, countSets = 0;
        Object.defineProperty(countCase[0], 'holdCount', {
          get: function() { countGets++; return count; },
          set: function(x) { countSets++; count = x; },
          configurable: true
        });
        countCase[0].schedule();

        var stateCase = oneHold(null), state = stateCase[2].state;
        var stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[2], 'state', {
          get: function() { stateGets++; return state; },
          set: function(x) { stateSets++; state = x; },
          configurable: true
        });
        stateCase[0].schedule();

        var linkCase = oneHold(null), link = linkCase[2].link, linkGets = 0;
        Object.defineProperty(linkCase[2], 'link', {
          get: function() { linkGets++; return link; }, configurable: true
        });
        linkCase[0].schedule();

        var overflow = oneHold(null);
        overflow[0].holdCount = 2147483647;
        overflow[0].schedule();

        [holdHits, markHits, globalResult,
         v1Gets, v1Sets, v1 === v1Case[3], v1Case[0].holdCount, v1Case[2].state,
         countGets, countSets, count,
         stateGets, stateSets, state,
         linkGets, linkCase[0].holdCount, linkCase[1].v1 === linkCase[3],
         overflow[0].holdCount].flat().join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(
        run_jit(&src),
        "928|928|1|8|true|true|8|0|1|true|1|4|1|1|1|4|2|4|1|1|true|2147483648"
    );
}

#[test]
fn jit_scheduler_device_queue_guards_ownership_descriptors_and_overflow() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();

        function oneQueue(link, queued, targetPriority, currentPriority) {
          var scheduler = new Scheduler();
          var device = new DeviceTask(scheduler);
          var packet = new Packet(link, ID_HANDLER_A, KIND_DEVICE);
          var targetTask = {
            seen: null,
            run: function(packet) { this.seen = packet; return null; }
          };
          var target = new TaskControlBlock(null, ID_HANDLER_A,
                                            targetPriority == null ? 1 : targetPriority,
                                            queued, targetTask);
          var current = new TaskControlBlock(null, ID_DEVICE_A,
                                             currentPriority == null ? 2 : currentPriority,
                                             null, device);
          current.state = STATE_RUNNING;
          for (var id = 0; id < NUMBER_OF_IDS; id++) scheduler.blocks[id] = target;
          scheduler.blocks[ID_DEVICE_A] = current;
          scheduler.list = current;
          device.v1 = packet;
          return [scheduler, device, current, target, packet, targetTask];
        }

        var oldLink = new Packet(null, ID_WORKER, KIND_WORK);
        oldLink.a1 = 77;
        var direct = oneQueue(oldLink, null, 1, 2);
        direct[0].schedule();
        var directResult = [direct[0].queueCount, direct[3].state,
                            direct[3].queue === direct[4], direct[4].link === null,
                            direct[4].id, direct[1].v1 === null, oldLink.a1,
                            direct[2].state, direct[0].currentTcb === null];

        var selfLink = oneQueue(null, null, 1, 2);
        selfLink[4].link = selfLink[4];
        selfLink[0].schedule();
        var selfLinkResult = [selfLink[0].queueCount,
                              selfLink[3].queue === selfLink[4],
                              selfLink[4].link === null,
                              selfLink[1].v1 === null,
                              selfLink[3].state,
                              selfLink[0].currentTcb === null];

        var lastOwner = oneQueue(new Packet(null, ID_WORKER, KIND_WORK),
                                 null, 1, 2);
        lastOwner[0].schedule();
        var lastOwnerResult = [lastOwner[0].queueCount,
                               lastOwner[3].queue === lastOwner[4],
                               lastOwner[4].link === null,
                               lastOwner[1].v1 === null];

        var preempt = oneQueue(null, null, 3, 2);
        preempt[0].schedule();
        var preemptResult = [preempt[0].queueCount,
                             preempt[5].seen === preempt[4],
                             preempt[3].queue === null,
                             preempt[3].state,
                             preempt[4].id,
                             preempt[4].link === null,
                             preempt[1].v1 === null,
                             preempt[0].currentTcb === null];

        var originalQueue = Scheduler.prototype.queue, queueHits = 0;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          return originalQueue.call(this, packet);
        };
        oneQueue(null, null, 1, 2)[0].schedule();
        Scheduler.prototype.queue = originalQueue;

        var originalCheck = TaskControlBlock.prototype.checkPriorityAdd, checkHits = 0;
        TaskControlBlock.prototype.checkPriorityAdd = function(task, packet) {
          checkHits++;
          return originalCheck.call(this, task, packet);
        };
        oneQueue(null, null, 1, 2)[0].schedule();
        TaskControlBlock.prototype.checkPriorityAdd = originalCheck;

        var originalMark = TaskControlBlock.prototype.markAsRunnable, markHits = 0;
        TaskControlBlock.prototype.markAsRunnable = function() {
          markHits++;
          return originalMark.call(this);
        };
        oneQueue(null, null, 1, 2)[0].schedule();
        TaskControlBlock.prototype.markAsRunnable = originalMark;

        var oldRunnable = STATE_RUNNABLE;
        STATE_RUNNABLE = 8;
        var globalCase = oneQueue(null, null, 1, 2);
        globalCase[0].schedule();
        var globalState = globalCase[3].state;
        STATE_RUNNABLE = oldRunnable;

        var holeLink = new Packet(null, ID_WORKER, KIND_WORK);
        var hole = oneQueue(holeLink, null, 1, 2);
        delete hole[0].blocks[ID_HANDLER_A];
        hole[0].schedule();
        var holeResult = [hole[0].queueCount, hole[3].queue === null,
                          hole[1].v1 === null, hole[4].link === holeLink,
                          hole[4].id];

        var tail = new Packet(null, ID_HANDLER_A, KIND_DEVICE);
        var queued = new Packet(tail, ID_HANDLER_A, KIND_DEVICE);
        var replacedLink = new Packet(null, ID_WORKER, KIND_WORK);
        var nonempty = oneQueue(replacedLink, queued, 1, 2);
        nonempty[0].schedule();
        var nonemptyResult = [nonempty[0].queueCount,
                              nonempty[3].queue === queued,
                              tail.link === nonempty[4],
                              nonempty[4].link === null,
                              nonempty[4].id,
                              nonempty[3].state,
                              replacedLink.id];

        var overflow = oneQueue(null, null, 1, 2);
        overflow[0].queueCount = 2147483647;
        overflow[0].schedule();
        var overflowCount = overflow[0].queueCount;

        var countCase = oneQueue(null, null, 1, 2), count = 0;
        var countGets = 0, countSets = 0;
        Object.defineProperty(countCase[0], 'queueCount', {
          get: function() { countGets++; return count; },
          set: function(x) { countSets++; count = x; }, configurable: true
        });
        countCase[0].schedule();

        var linkCase = oneQueue(null, null, 1, 2), storedLink = oldLink;
        var linkGets = 0, linkSets = 0;
        Object.defineProperty(linkCase[4], 'link', {
          get: function() { linkGets++; return storedLink; },
          set: function(x) { linkSets++; storedLink = x; }, configurable: true
        });
        linkCase[0].schedule();

        var idCase = oneQueue(null, null, 1, 2), storedId = ID_HANDLER_A;
        var idGets = 0, idSets = 0;
        Object.defineProperty(idCase[4], 'id', {
          get: function() { idGets++; return storedId; },
          set: function(x) { idSets++; storedId = x; }, configurable: true
        });
        idCase[0].schedule();

        var blocksCase = oneQueue(null, null, 1, 2), storedBlocks = blocksCase[0].blocks;
        var blocksGets = 0;
        Object.defineProperty(blocksCase[0], 'blocks', {
          get: function() { blocksGets++; return storedBlocks; }, configurable: true
        });
        blocksCase[0].schedule();

        var targetQueueCase = oneQueue(null, null, 1, 2), storedQueue = null;
        var queueGets = 0, queueSets = 0;
        Object.defineProperty(targetQueueCase[3], 'queue', {
          get: function() { queueGets++; return storedQueue; },
          set: function(x) { queueSets++; storedQueue = x; }, configurable: true
        });
        targetQueueCase[0].schedule();

        var stateCase = oneQueue(null, null, 1, 2), storedState = stateCase[3].state;
        var stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[3], 'state', {
          get: function() { stateGets++; return storedState; },
          set: function(x) { stateSets++; storedState = x; }, configurable: true
        });
        stateCase[0].schedule();

        var priorityCase = oneQueue(null, null, 1, 2);
        var targetPriority = priorityCase[3].priority;
        var currentPriority = priorityCase[2].priority;
        var targetPriorityGets = 0, currentPriorityGets = 0;
        Object.defineProperty(priorityCase[3], 'priority', {
          get: function() { targetPriorityGets++; return targetPriority; },
          configurable: true
        });
        Object.defineProperty(priorityCase[2], 'priority', {
          get: function() { currentPriorityGets++; return currentPriority; },
          configurable: true
        });
        priorityCase[0].schedule();

        [directResult, selfLinkResult, lastOwnerResult, preemptResult,
         queueHits, checkHits, markHits, globalState,
         holeResult, nonemptyResult, overflowCount,
         countGets, countSets, count,
         linkGets, linkSets, storedLink === null, oldLink.a1,
         idGets, idSets, storedId,
         blocksGets, blocksCase[3].queue === blocksCase[4],
         queueGets, queueSets, storedQueue === targetQueueCase[4],
         stateGets, stateSets, storedState,
         targetPriorityGets, currentPriorityGets].flat().join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(
        run_jit(&src),
        "1|3|true|true|4|true|77|2|true|1|true|true|true|3|true|1|true|true|true|1|true|true|0|4|true|true|true|1|1|1|10|0|true|true|true|2|1|true|true|true|4|3|1|2147483648|1|1|1|0|1|true|77|1|1|4|1|true|1|1|true|1|1|3|1|1"
    );
}

#[test]
fn jit_scheduler_active_handler_null_full_transfers_delivery_and_completion_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler null owners: ' + message;
        }
        function makeCase(count, payload, workLink, packetLink, queued,
                          targetPriority, holdTarget) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(workLink, ID_WORKER, KIND_WORK);
          work.a1 = count;
          work.a2[count] = payload;
          var packet = new Packet(packetLink, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(
              null, ID_WORKER, targetPriority, queued, { run: function() {
                throw 'held/off-list target ran';
              }});
          if (holdTarget) target.state = target.state | STATE_HELD;
          var current = new TaskControlBlock(
              null, ID_HANDLER_A, 2, null, handler);
          current.state = STATE_RUNNING;
          scheduler.blocks[ID_WORKER] = target;
          // A later completed Handler packet deliberately terminates the isolated schedule.
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          handler.v1 = work;
          handler.v2 = packet;
          return {
            scheduler: scheduler, handler: handler, work: work, packet: packet,
            current: current, target: target, queued: queued,
            workLink: workLink, packetLink: packetLink
          };
        }

        // One-node delivery keeps current unchanged and can take the graph fast-resume edge.
        // The following completed-work miss terminates without disturbing the appended owner.
        var queued = new Packet(null, ID_WORKER, KIND_DEVICE);
        var oneNode = makeCase(DATA_SIZE - 1, 71, null, null, queued, 1, false);
        oneNode.scheduler.schedule();
        check(oneNode.scheduler.queueCount === 2 && queued.link === oneNode.packet &&
              oneNode.packet.link === oneNode.work && oneNode.work.link === null,
              'one-node delivery/completion publication');
        check(oneNode.packet.a1 === 71 &&
              oneNode.packet.id === ID_HANDLER_A, 'one-node packet writes');
        check(oneNode.handler.v2 === null && oneNode.handler.v1 === null &&
              oneNode.work.a1 === DATA_SIZE, 'one-node Handler writes');

        // Empty/preempting delivery stops on a held target. P.link's sole list owner moves to
        // Handler.v2 while the former Handler.v2 owner moves into target.queue.
        var packetTail = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        packetTail.a2[0] = 81;
        var preempt = makeCase(1, 72, null, packetTail, null, 3, true);
        preempt.scheduler.schedule();
        check(preempt.handler.v2 === packetTail && packetTail.a2[0] === 81,
              'delivery successor owner');
        check(preempt.target.queue === preempt.packet && preempt.packet.link === null &&
              preempt.packet.a1 === 72, 'delivery packet owner');
        check(preempt.current.state === STATE_RUNNING &&
              preempt.scheduler.currentTcb === null, 'delivery preempt completion');

        // Completed-v1 queue transfers W.link into Handler.v1 and W into target.queue without
        // transient retains. The next Handler iteration observes the successor and suspends.
        var workTail = new Packet(null, ID_HANDLER_A, KIND_WORK);
        workTail.a1 = 0;
        workTail.a2[0] = 82;
        var completed = makeCase(DATA_SIZE, 73, workTail, null, null, 1, true);
        completed.handler.v2 = null;
        completed.scheduler.schedule();
        check(completed.handler.v1 === workTail && workTail.a2[0] === 82,
              'completion successor owner');
        check(completed.target.queue === completed.work && completed.work.link === null &&
              completed.work.id === ID_HANDLER_A, 'completion packet owner');
        check(completed.scheduler.queueCount === 1 &&
              completed.current.state === STATE_SUSPENDED, 'completion scheduler writes');

        // W=P is intentionally outside the direct delivery transaction. Untouched pc59 replay
        // must preserve the source-ordered double a1 write and publish exactly one packet.
        var aliasQueued = new Packet(null, ID_WORKER, KIND_DEVICE);
        var alias = makeCase(3, 74, null, null, aliasQueued, 1, false);
        alias.handler.v1 = alias.packet;
        alias.packet.a1 = 3;
        alias.packet.a2[3] = 74;
        alias.scheduler.schedule();
        check(alias.handler.v1 === null && alias.handler.v2 === null &&
              alias.packet.a1 === DATA_SIZE, 'delivery alias write order');
        check(aliasQueued.link === alias.packet && alias.packet.link === null &&
              alias.scheduler.queueCount === 1, 'delivery alias owner');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_null_full_replays_live_globals_accessors_and_methods() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler null guards: ' + message;
        }
        function oneDelivery(count, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_WORKER, KIND_WORK);
          work.a1 = count;
          work.a2[count] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 3, null, {
            run: function() { throw 'held target ran'; }
          });
          target.state = target.state | STATE_HELD;
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, null, handler);
          current.state = STATE_RUNNING;
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          handler.v1 = work;
          handler.v2 = packet;
          return {
            scheduler: scheduler, handler: handler, work: work, packet: packet,
            current: current, target: target, payload: payload
          };
        }

        // DATA_SIZE is guarded live. An exact changed integer selects completion; a fractional
        // value declines the stitch and executes the original numeric comparison/delivery.
        var oldDataSize = DATA_SIZE;
        var changedSize = oneDelivery(1, 83);
        DATA_SIZE = 1;
        changedSize.scheduler.schedule();
        DATA_SIZE = oldDataSize;
        check(changedSize.target.queue === changedSize.work &&
              changedSize.handler.v1 === null && changedSize.handler.v2 === changedSize.packet,
              'changed integer DATA_SIZE');

        var fractional = oneDelivery(1, 84);
        DATA_SIZE = 4.5;
        fractional.scheduler.schedule();
        DATA_SIZE = oldDataSize;
        check(fractional.target.queue === fractional.packet &&
              fractional.handler.v2 === null && fractional.work.a1 === 2 &&
              fractional.packet.a1 === 84, 'fractional DATA_SIZE replay');

        // Accessor shapes must fall back before the stitch writes anything. Original Handler
        // source order performs three v2 gets and one set for a delivery.
        var v2Case = oneDelivery(1, 85), storedV2 = v2Case.packet;
        var v2Gets = 0, v2Sets = 0;
        Object.defineProperty(v2Case.handler, 'v2', {
          get: function() { v2Gets++; return storedV2; },
          set: function(value) { v2Sets++; storedV2 = value; }, configurable: true
        });
        v2Case.scheduler.schedule();
        check(v2Gets === 3 && v2Sets === 1 && storedV2 === null &&
              v2Case.packet.a1 === 85, 'v2 accessor replay once');

        var a1Case = oneDelivery(1, 86), storedA1 = 1;
        var a1Gets = 0, a1Sets = 0;
        Object.defineProperty(a1Case.work, 'a1', {
          get: function() { a1Gets++; return storedA1; },
          set: function(value) { a1Sets++; storedA1 = value; }, configurable: true
        });
        a1Case.scheduler.schedule();
        check(a1Gets === 1 && a1Sets === 1 && storedA1 === 2 &&
              a1Case.packet.a1 === 86, 'a1 accessor replay once');

        var schedulerCase = oneDelivery(1, 87), schedulerGets = 0;
        var storedScheduler = schedulerCase.scheduler;
        Object.defineProperty(schedulerCase.handler, 'scheduler', {
          get: function() { schedulerGets++; return storedScheduler; }, configurable: true
        });
        schedulerCase.scheduler.schedule();
        check(schedulerGets === 1 && schedulerCase.packet.a1 === 87,
              'scheduler accessor replay once');

        // Changed task and nested scheduler methods are resolved in original order, once, and
        // see the same pre-call state as the interpreter path.
        var originalRun = HandlerTask.prototype.run, runHits = 0;
        HandlerTask.prototype.run = function(packet) {
          runHits++;
          return originalRun.call(this, packet);
        };
        var runCase = oneDelivery(1, 88);
        runCase.scheduler.schedule();
        HandlerTask.prototype.run = originalRun;
        check(runHits === 1 && runCase.packet.a1 === 88, 'run method replay once');

        var originalQueue = Scheduler.prototype.queue, queueHits = 0;
        var queueCase = oneDelivery(1, 89), queueSawWrites = false;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          queueSawWrites = queueCase.handler.v2 === null &&
                           queueCase.work.a1 === 2 && packet.a1 === 89 &&
                           this.queueCount === 0;
          return originalQueue.call(this, packet);
        };
        queueCase.scheduler.schedule();
        Scheduler.prototype.queue = originalQueue;
        check(queueHits === 1 && queueSawWrites, 'queue method replay once');

        // Handler.scheduler may name a different Scheduler. Equality failure must occur before
        // delivery writes, then ordinary execution mutates only that foreign receiver.
        var foreignCase = oneDelivery(1, 90), foreign = new Scheduler();
        foreign.blocks[ID_WORKER] = foreignCase.target;
        foreign.currentTcb = foreignCase.current;
        foreign.currentId = 99;
        foreignCase.handler.scheduler = foreign;
        foreignCase.scheduler.schedule();
        check(foreign.queueCount === 1 && foreignCase.scheduler.queueCount === 0 &&
              foreignCase.packet.id === 99 && foreignCase.packet.a1 === 90,
              'foreign scheduler receiver');
        check(foreignCase.target.queue === foreignCase.packet &&
              foreignCase.handler.v2 === null && foreignCase.work.a1 === 2,
              'foreign scheduler writes once');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_wait_suspend_guards_live_values_and_descriptors() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();

        var originalRun = HandlerTask.prototype.run, runHits = 0;
        HandlerTask.prototype.run = function(packet) {
          runHits++;
          return originalRun.call(this, packet);
        };
        runRichards();
        HandlerTask.prototype.run = originalRun;

        var originalSuspend = Scheduler.prototype.suspendCurrent, suspendHits = 0;
        Scheduler.prototype.suspendCurrent = function() {
          suspendHits++;
          return originalSuspend.call(this);
        };
        runRichards();
        Scheduler.prototype.suspendCurrent = originalSuspend;

        var originalMark = TaskControlBlock.prototype.markAsSuspended, markHits = 0;
        TaskControlBlock.prototype.markAsSuspended = function() {
          markHits++;
          return originalMark.call(this);
        };
        runRichards();
        TaskControlBlock.prototype.markAsSuspended = originalMark;

        function oneWait(a1, v2) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          if (a1 !== null) {
            var packet = new Packet(null, ID_HANDLER_A, KIND_WORK);
            packet.a1 = a1;
            handler.v1 = packet;
          } else {
            var packet = null;
          }
          handler.v2 = v2;
          var current = new TaskControlBlock(null, ID_HANDLER_A, 1, null, handler);
          current.state = STATE_RUNNING;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          return [scheduler, handler, current, packet];
        }

        var nullCase = oneWait(null, null);
        nullCase[0].schedule();

        var oldLink = new Packet(null, ID_WORKER, KIND_WORK);
        var objectCase = oneWait(1, null);
        objectCase[3].link = oldLink;
        objectCase[0].schedule();

        var oldDataSize = DATA_SIZE;
        var fractionalGlobal = oneWait(1, null);
        DATA_SIZE = 4.5;
        fractionalGlobal[0].schedule();
        DATA_SIZE = oldDataSize;

        var fractionalA1 = oneWait(1.5, null);
        fractionalA1[0].schedule();

        var oldSuspended = STATE_SUSPENDED;
        STATE_SUSPENDED = 8;
        var globalState = oneWait(1, null);
        globalState[0].schedule();
        STATE_SUSPENDED = oldSuspended;

        var undefinedCase = oneWait(null, null);
        undefinedCase[1].v1 = undefined;
        undefinedCase[0].schedule();
        var ddaCase = oneWait(null, null);
        ddaCase[1].v1 = $262.IsHTMLDDA;
        ddaCase[0].schedule();

        var v1Case = oneWait(null, null), storedV1 = null, v1Gets = 0;
        Object.defineProperty(v1Case[1], 'v1', {
          get: function() { v1Gets++; return storedV1; }, configurable: true
        });
        v1Case[0].schedule();

        var a1Case = oneWait(1, null), storedA1 = 1, a1Gets = 0;
        Object.defineProperty(a1Case[3], 'a1', {
          get: function() { a1Gets++; return storedA1; }, configurable: true
        });
        a1Case[0].schedule();

        var v2Case = oneWait(1, null), storedV2 = null, v2Gets = 0;
        Object.defineProperty(v2Case[1], 'v2', {
          get: function() { v2Gets++; return storedV2; }, configurable: true
        });
        v2Case[0].schedule();

        var schedulerCase = oneWait(1, null), storedScheduler = schedulerCase[0];
        var schedulerGets = 0;
        Object.defineProperty(schedulerCase[1], 'scheduler', {
          get: function() { schedulerGets++; return storedScheduler; }, configurable: true
        });
        schedulerCase[0].schedule();

        var stateCase = oneWait(1, null), storedState = STATE_RUNNING;
        var stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[2], 'state', {
          get: function() { stateGets++; return storedState; },
          set: function(x) { stateSets++; storedState = x; }, configurable: true
        });
        stateCase[0].schedule();

        var currentCase = oneWait(1, null), storedCurrent = currentCase[0].currentTcb;
        var currentGets = 0, currentSets = 0;
        Object.defineProperty(currentCase[0], 'currentTcb', {
          get: function() { currentGets++; return storedCurrent; },
          set: function(x) { currentSets++; storedCurrent = x; }, configurable: true
        });
        currentCase[0].schedule();

        [runHits, suspendHits, markHits,
         nullCase[2].state, nullCase[0].currentTcb === null,
         objectCase[2].state, objectCase[1].v1 === objectCase[3],
         objectCase[3].a1, objectCase[3].link === oldLink,
         objectCase[0].currentTcb === null,
         fractionalGlobal[2].state, fractionalA1[2].state,
         globalState[2].state, undefinedCase[2].state, ddaCase[2].state,
         v1Gets, v1Case[2].state,
         a1Gets, a1Case[2].state,
         v2Gets, v2Case[2].state,
         schedulerGets, schedulerCase[2].state,
         stateGets > 0, stateSets, storedState,
         currentGets > 0, currentSets > 0, storedCurrent === null].join('|')
        "#,
    ]
    .join("\n");
    assert_eq!(
        run_jit(&src),
        "2328|2324|2324|2|true|2|true|1|true|true|2|2|8|2|2|1|2|1|2|1|2|1|2|true|1|2|true|true|true"
    );
}

#[test]
fn jit_scheduler_handler_queue_transfers_owners_and_replays_before_effects() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();

        function check(value, message) {
          if (!value) throw 'handler queue: ' + message;
        }
        function linked(a1) {
          var packet = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
          packet.a1 = a1;
          return packet;
        }
        function oneHandlerQueue(link, queued, targetPriority, currentPriority, a1) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var packet = new Packet(link, ID_WORKER, KIND_WORK);
          packet.a1 = a1 === undefined ? DATA_SIZE : a1;
          var targetTask = {
            seen: null,
            run: function(packet) { this.seen = packet; return null; }
          };
          var target = new TaskControlBlock(null, ID_WORKER,
                                            targetPriority == null ? 1 : targetPriority,
                                            queued, targetTask);
          var current = new TaskControlBlock(null, ID_HANDLER_A,
                                             currentPriority == null ? 2 : currentPriority,
                                             null, handler);
          current.state = STATE_RUNNING;
          scheduler.blocks[ID_WORKER] = target;
          // A Handler self-link queues once, then the rewritten Handler id deliberately misses.
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          handler.v1 = packet;
          return [scheduler, handler, current, target, packet, targetTask];
        }

        var direct = oneHandlerQueue(null, null);
        direct[0].schedule();
        check(direct[0].queueCount === 1, 'direct count');
        check(direct[3].queue === direct[4] && direct[3].state === 3, 'direct target');
        check(direct[1].v1 === null && direct[4].link === null, 'direct source');
        check(direct[2].state === STATE_SUSPENDED && direct[0].currentTcb === null,
              'direct suspension');

        var oldLink = linked(1);
        oldLink.a2[0] = 77;
        var transfer = oneHandlerQueue(oldLink, null);
        transfer[0].schedule();
        check(transfer[1].v1 === oldLink && oldLink.a2[0] === 77, 'object transfer');
        check(transfer[3].queue === transfer[4] && transfer[4].link === null,
              'object packet move');

        var selfLink = oneHandlerQueue(null, null);
        selfLink[4].link = selfLink[4];
        selfLink[0].schedule();
        check(selfLink[0].queueCount === 1, 'self count');
        check(selfLink[3].queue === selfLink[4] && selfLink[4].link === null,
              'self packet');
        check(selfLink[1].v1 === null && selfLink[0].currentTcb === null, 'self source');

        var lastOwner = oneHandlerQueue(null, null);
        lastOwner[4].link = linked(1);
        lastOwner[4].link.a2[0] = 91;
        lastOwner[0].schedule();
        check(lastOwner[1].v1.a2[0] === 91 && lastOwner[4].link === null,
              'last-owner link');

        var missingLink = linked(2);
        var missing = oneHandlerQueue(missingLink, null);
        missing[0].blocks[ID_WORKER] = null;
        missing[0].schedule();
        check(missing[0].queueCount === 0 && missing[1].v1 === missingLink,
              'missing target source effect');
        check(missing[4].link === missingLink && missing[4].id === ID_WORKER,
              'missing target packet untouched');

        var queued = linked(0), replaced = linked(1);
        var nonempty = oneHandlerQueue(replaced, queued);
        nonempty[0].schedule();
        check(nonempty[3].queue === queued && queued.link === nonempty[4],
              'nonempty append');
        check(nonempty[1].v1 === replaced && nonempty[4].link === null,
              'nonempty source');

        var preempt = oneHandlerQueue(null, null, 3, 2);
        preempt[0].schedule();
        check(preempt[0].queueCount === 1 && preempt[5].seen === preempt[4],
              'preempt run');
        check(preempt[3].queue === null && preempt[3].state === STATE_RUNNING,
              'preempt consume');

        var undefinedLink = oneHandlerQueue(null, null);
        undefinedLink[4].link = undefined;
        undefinedLink[0].schedule();
        check(undefinedLink[1].v1 === undefined && undefinedLink[3].queue === undefinedLink[4],
              'undefined link fallback');
        var ddaLink = oneHandlerQueue(null, null);
        ddaLink[4].link = $262.IsHTMLDDA;
        ddaLink[0].schedule();
        check(ddaLink[1].v1 === $262.IsHTMLDDA && ddaLink[3].queue === ddaLink[4],
              'HTMLDDA link fallback');

        var undefinedQueue = oneHandlerQueue(null, undefined);
        undefinedQueue[0].schedule();
        check(undefinedQueue[3].queue === undefinedQueue[4], 'undefined target queue');
        var ddaQueue = oneHandlerQueue(null, $262.IsHTMLDDA);
        ddaQueue[0].schedule();
        check(ddaQueue[3].queue === ddaQueue[4], 'HTMLDDA target queue');

        var originalRun = HandlerTask.prototype.run, runHits = 0;
        HandlerTask.prototype.run = function(packet) {
          runHits++;
          return originalRun.call(this, packet);
        };
        var runCase = oneHandlerQueue(null, null);
        runCase[0].schedule();
        HandlerTask.prototype.run = originalRun;
        check(runHits > 0 && runCase[3].queue === runCase[4], 'run replacement');

        var originalQueue = Scheduler.prototype.queue, queueHits = 0;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          return originalQueue.call(this, packet);
        };
        var queueCase = oneHandlerQueue(null, null);
        queueCase[0].schedule();
        Scheduler.prototype.queue = originalQueue;
        check(queueHits === 1 && queueCase[3].queue === queueCase[4], 'queue replacement');

        var originalCheck = TaskControlBlock.prototype.checkPriorityAdd, checkHits = 0;
        TaskControlBlock.prototype.checkPriorityAdd = function(task, packet) {
          checkHits++;
          return originalCheck.call(this, task, packet);
        };
        var checkCase = oneHandlerQueue(null, null);
        checkCase[0].schedule();
        TaskControlBlock.prototype.checkPriorityAdd = originalCheck;
        check(checkHits === 1 && checkCase[3].queue === checkCase[4], 'check replacement');

        var originalMark = TaskControlBlock.prototype.markAsRunnable, markHits = 0;
        TaskControlBlock.prototype.markAsRunnable = function() {
          markHits++;
          return originalMark.call(this);
        };
        var markCase = oneHandlerQueue(null, null);
        markCase[0].schedule();
        TaskControlBlock.prototype.markAsRunnable = originalMark;
        check(markHits === 1 && markCase[3].state === 3, 'mark replacement');

        var oldRunnable = STATE_RUNNABLE;
        STATE_RUNNABLE = 8;
        var runnableCase = oneHandlerQueue(null, null);
        runnableCase[0].schedule();
        STATE_RUNNABLE = oldRunnable;
        check(runnableCase[3].state === 10, 'live runnable global');

        var oldDataSize = DATA_SIZE;
        DATA_SIZE = 5;
        var waitCase = oneHandlerQueue(null, null, 1, 2, 4);
        waitCase[0].schedule();
        DATA_SIZE = oldDataSize;
        check(waitCase[0].queueCount === 0 && waitCase[1].v1 === waitCase[4],
              'live DATA_SIZE branch');

        var v1Case = oneHandlerQueue(null, null), storedV1 = v1Case[4];
        var v1Gets = 0, v1Sets = 0;
        Object.defineProperty(v1Case[1], 'v1', {
          get: function() { v1Gets++; return storedV1; },
          set: function(value) { v1Sets++; storedV1 = value; }, configurable: true
        });
        v1Case[0].schedule();
        check(v1Gets > 0 && v1Sets > 0 && storedV1 === null, 'v1 accessor');

        var linkCase = oneHandlerQueue(null, null), storedLink = linked(1);
        var linkGets = 0, linkSets = 0;
        Object.defineProperty(linkCase[4], 'link', {
          get: function() { linkGets++; return storedLink; },
          set: function(value) { linkSets++; storedLink = value; }, configurable: true
        });
        linkCase[0].schedule();
        check(linkGets > 0 && linkSets > 0 && storedLink === null, 'link accessor');
        check(linkCase[1].v1.a1 === 1, 'link accessor transfer');

        var schedulerCase = oneHandlerQueue(null, null), storedScheduler = schedulerCase[0];
        var schedulerGets = 0;
        Object.defineProperty(schedulerCase[1], 'scheduler', {
          get: function() { schedulerGets++; return storedScheduler; }, configurable: true
        });
        schedulerCase[0].schedule();
        check(schedulerGets > 0 && schedulerCase[3].queue === schedulerCase[4],
              'scheduler accessor');

        var currentCase = oneHandlerQueue(null, null), storedCurrent = null;
        var currentGets = 0, currentSets = 0;
        Object.defineProperty(currentCase[0], 'currentTcb', {
          get: function() { currentGets++; return storedCurrent; },
          set: function(value) { currentSets++; storedCurrent = value; }, configurable: true
        });
        currentCase[0].schedule();
        check(currentGets > 0 && currentSets > 0 && storedCurrent === null,
              'current accessor');
        check(currentCase[3].queue === currentCase[4], 'current accessor effects');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_work_delivery_moves_source_and_v2_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active Handler WORK delivery: ' + message;
        }
        function oneDelivery(depth, preempt, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 0;
          work.a2[0] = payload;
          var devices = null;
          for (var i = 0; i < depth; i++)
            devices = new Packet(devices, ID_WORKER, KIND_DEVICE);
          handler.v2 = devices;
          var queued = preempt ? null : new Packet(null, ID_WORKER, KIND_DEVICE);
          var targetTask = {
            seen: null,
            run: function(packet) { this.seen = packet; return null; }
          };
          var target = new TaskControlBlock(null, ID_WORKER, preempt ? 3 : 1,
                                            queued, targetTask);
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, work, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          // W and every D are intentionally omitted. Their only source owners must move through
          // C.queue/H.v1 and H.v2/target without a retain or last-owner gap.
          return [scheduler, handler, current, target, targetTask, queued];
        }

        var one = oneDelivery(1, false, 71), q = one[5];
        one[0].schedule();
        var d1 = q.link, w1 = one[1].v1;
        check(d1 !== null && d1.link === null && d1.a1 === 71 &&
              d1.id === ID_HANDLER_A, 'depth-one delivered owner');
        check(w1 !== null && w1.a1 === 1 && w1.link === null &&
              one[1].v2 === null, 'depth-one work/source owners');
        check(one[3].queue === q && one[2].queue === null &&
              one[2].state === STATE_SUSPENDED && one[0].queueCount === 1,
              'one-node nonpreempt');

        var two = oneDelivery(2, true, 72);
        two[0].schedule();
        var first2 = two[4].seen, rest2 = two[1].v2, work2 = two[1].v1;
        check(first2 !== null && first2.link === null && first2.a1 === 72 &&
              first2.id === ID_HANDLER_A, 'depth-two delivered owner');
        check(rest2 !== null && rest2.link === null && work2 !== null &&
              work2.a1 === 1 && work2.link === null, 'depth-two successor/source owners');
        check(two[3].queue === null && two[2].queue === null &&
              two[2].state === STATE_RUNNING && two[0].queueCount === 1,
              'depth-two empty preempt');

        var three = oneDelivery(3, true, 73);
        three[0].schedule();
        var first3 = three[4].seen, rest3 = three[1].v2, work3 = three[1].v1;
        check(first3 !== null && first3.link === null && first3.a1 === 73 &&
              first3.id === ID_HANDLER_A, 'depth-three delivered owner');
        check(rest3 !== null && rest3.link !== null && rest3.link.link === null &&
              work3 !== null && work3.a1 === 1 && work3.link === null,
              'depth-three successor/source owners');
        check(three[2].state === STATE_RUNNING && three[0].currentTcb === null &&
              three[0].queueCount === 1, 'depth-three graph rebuild');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_work_delivery_replays_late_guards_and_aliases() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active Handler WORK replay: ' + message;
        }
        function oneDelivery(preempt, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 0;
          work.a2[0] = payload;
          var device = new Packet(null, ID_WORKER, KIND_DEVICE);
          handler.v2 = device;
          var queued = preempt ? null : new Packet(null, ID_WORKER, KIND_DEVICE);
          var task = { seen: null, run: function(p) { this.seen = p; return null; } };
          var target = new TaskControlBlock(null, ID_WORKER, preempt ? 3 : 1,
                                            queued, task);
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, work, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          return [scheduler, handler, current, target, task, queued, work, device];
        }

        // W.a1 is read after incoming addTo. Generic replay must expose H.v1=W but leave H.v2,
        // D, target, and queueCount untouched when the accessor throws exactly once.
        var a1Case = oneDelivery(true, 81), a1Gets = 0, storedA1 = 0;
        Object.defineProperty(a1Case[6], 'a1', {
          get: function() { a1Gets++; throw 'a1'; },
          set: function(v) { storedA1 = v; }, configurable: true
        });
        var a1Error = '';
        try { a1Case[0].schedule(); } catch (e) { a1Error = e; }
        check(a1Error === 'a1' && a1Gets === 1 && a1Case[1].v1 === a1Case[6],
              'a1 accessor replay once');
        check(a1Case[1].v2 === a1Case[7] && a1Case[4].seen === null &&
              a1Case[0].queueCount === 0 && a1Case[2].queue === null &&
              a1Case[2].state === STATE_RUNNING, 'a1 checkpoint');

        // The indexed payload read follows H.v2 advancement in source order.
        var elemCase = oneDelivery(true, 82), elemGets = 0;
        Object.defineProperty(elemCase[6].a2, 0, {
          get: function() { elemGets++; throw 'elem'; }, configurable: true
        });
        var elemError = '';
        try { elemCase[0].schedule(); } catch (e) { elemError = e; }
        check(elemError === 'elem' && elemGets === 1 && elemCase[1].v1 === elemCase[6] &&
              elemCase[1].v2 === null, 'payload accessor source order');
        check(elemCase[7].a1 === 0 && elemCase[4].seen === null &&
              elemCase[0].queueCount === 0, 'payload checkpoint');

        var originalAdd = Packet.prototype.addTo, addHits = 0;
        Packet.prototype.addTo = function(queue) {
          addHits++;
          return originalAdd.call(this, queue);
        };
        var addCase = oneDelivery(true, 83);
        addCase[0].schedule();
        Packet.prototype.addTo = originalAdd;
        check(addHits === 1 && addCase[4].seen === addCase[7],
              'incoming addTo replacement once');

        var originalQueue = Scheduler.prototype.queue, queueHits = 0;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          return originalQueue.call(this, packet);
        };
        var queueCase = oneDelivery(true, 84);
        queueCase[0].schedule();
        Scheduler.prototype.queue = originalQueue;
        check(queueHits === 1 && queueCase[4].seen === queueCase[7] &&
              queueCase[7].a1 === 84, 'queue replacement once');

        // A non-Null incoming link is moved by Active into C.queue before Handler clears W.link.
        // It is outside the narrow direct subset and must survive generic delivery/preemption.
        var linkCase = oneDelivery(true, 85);
        var successor = new Packet(null, ID_HANDLER_A, KIND_WORK);
        linkCase[6].link = successor;
        linkCase[0].schedule();
        check(linkCase[2].queue === successor && linkCase[6].link === null &&
              linkCase[2].state === STATE_RUNNABLE && linkCase[4].seen === linkCase[7],
              'incoming successor owner replay');

        // Preexisting H.v1 forces the full addTo path before delivery from H.v2.
        var oldCase = oneDelivery(true, 86);
        var oldWork = new Packet(null, ID_HANDLER_A, KIND_WORK);
        oldWork.a1 = 0;
        oldWork.a2[0] = 86;
        oldCase[1].v1 = oldWork;
        oldCase[0].schedule();
        check(oldCase[1].v1 === oldWork && oldWork.link === oldCase[6] &&
              oldWork.a1 === 1 && oldCase[6].link === null,
              'preexisting v1 replay');
        check(oldCase[4].seen === oldCase[7] && oldCase[7].a1 === 86,
              'preexisting v1 delivery');

        // D===W is valid source behavior. The second a1 store wins after the payload copy.
        var aliasScheduler = new Scheduler();
        var aliasHandler = new HandlerTask(aliasScheduler);
        var aliasPacket = new Packet(null, ID_WORKER, KIND_WORK);
        aliasPacket.a1 = 0;
        aliasPacket.a2[0] = 99;
        aliasHandler.v2 = aliasPacket;
        var aliasTask = { seen: null, run: function(p) { this.seen = p; return null; } };
        var aliasTarget = new TaskControlBlock(null, ID_WORKER, 3, null, aliasTask);
        var aliasCurrent = new TaskControlBlock(null, ID_HANDLER_A, 2,
                                                aliasPacket, aliasHandler);
        aliasScheduler.blocks[ID_WORKER] = aliasTarget;
        aliasScheduler.blocks[ID_HANDLER_A] = aliasCurrent;
        aliasScheduler.list = aliasCurrent;
        aliasScheduler.schedule();
        check(aliasTask.seen === aliasPacket && aliasPacket.a1 === 1 &&
              aliasPacket.id === ID_HANDLER_A && aliasPacket.link === null,
              'D equals W source order');
        check(aliasHandler.v1 === aliasPacket && aliasHandler.v2 === null &&
              aliasScheduler.queueCount === 1, 'D equals W graph');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_work_delivery_parity_case() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function oneDelivery(preempt, depth, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a2[0] = payload;
          var devices = null;
          for (var i = 0; i < depth; i++)
            devices = new Packet(devices, ID_WORKER, KIND_DEVICE);
          handler.v2 = devices;
          var queued = preempt ? null : new Packet(null, ID_WORKER, KIND_DEVICE);
          var task = { seen: null, run: function(p) { this.seen = p; return null; } };
          var target = new TaskControlBlock(null, ID_WORKER, preempt ? 3 : 1,
                                            queued, task);
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, work, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          scheduler.schedule();
          var delivered = preempt ? task.seen : queued.link;
          return [scheduler.queueCount, current.queue === null, current.state,
                  handler.v1 === work, work.a1, work.link === null,
                  delivered.a1, delivered.id, delivered.link === null,
                  handler.v2 === null ? 0 : 1].join('|');
        }
        oneDelivery(false, 1, 91) + ';' + oneDelivery(true, 2, 92)
        "#,
    ]
    .join("\n");
    assert_eq!(
        run_jit(&src),
        "1|true|2|true|1|true|91|2|true|0;1|true|0|true|1|true|92|2|true|1"
    );
}

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[test]
fn jit_scheduler_active_handler_incoming_work_delivery_enabled_disabled_parity() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    for disabled in [false, true] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::jit_scheduler_active_handler_incoming_work_delivery_parity_case")
            .arg("--nocapture")
            .env("LUMEN_JIT_REGIONLOG", "1")
            .env_remove("LUMEN_JIT_NO_SCHED_HANDLER_ACTIVE_WORK_DELIVERY");
        if disabled {
            command.env("LUMEN_JIT_NO_SCHED_HANDLER_ACTIVE_WORK_DELIVERY", "1");
        }
        let output = command.output().expect("run parity child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let planned = if disabled {
            "incoming_work_delivery=false"
        } else {
            "incoming_work_delivery=true"
        };
        assert!(
            output.status.success() && stdout.contains("running 1 test") && stderr.contains(planned),
            "Handler Active WORK delivery parity child disabled={disabled} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn jit_scheduler_active_handler_incoming_suspend_moves_bounded_packet_pools() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active Handler incoming suspend: ' + message;
        }
        function deviceBurst() {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var p3 = new Packet(null, ID_WORKER, KIND_DEVICE);
          var p2 = new Packet(p3, ID_WORKER, KIND_DEVICE);
          var p1 = new Packet(p2, ID_WORKER, KIND_DEVICE);
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, p1, handler);
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          // Do not return any packet root. Each incoming C.queue owner must move into v2/the
          // previous tail before its source is removed, including across same-record resumes.
          return [scheduler, handler, current];
        }
        var devices = deviceBurst();
        devices[0].schedule();
        var p1 = devices[1].v2, p2 = p1.link, p3 = p2.link;
        check(p1 !== null && p2 !== null && p3 !== null && p3.link === null,
              'DEVICE depth 0/1/2 chain');
        check(p1.id === ID_WORKER && p2.id === ID_WORKER && p3.id === ID_WORKER,
              'DEVICE owners survived');
        check(devices[2].queue === null && devices[2].state === STATE_SUSPENDED,
              'null successor final state');
        check(devices[0].currentId === ID_HANDLER_A &&
              devices[0].currentTcb === null && devices[0].queueCount === 0,
              'DEVICE scheduler result');

        // A non-Null successor makes the collapsed run+suspend state exactly
        // SUSPENDED_RUNNABLE. The next packet's throwing link getter checkpoints that state
        // before its own TaskControlBlock.run can advance the queue.
        var linkGets = 0;
        var successor = {};
        Object.defineProperty(successor, 'link', {
          get: function() { linkGets++; throw 'successor link'; }, configurable: true
        });
        var successorScheduler = new Scheduler();
        var successorHandler = new HandlerTask(successorScheduler);
        var first = new Packet(successor, ID_WORKER, KIND_DEVICE);
        var successorCurrent = new TaskControlBlock(
            null, ID_HANDLER_A, 2, first, successorHandler);
        successorScheduler.blocks[ID_HANDLER_A] = successorCurrent;
        successorScheduler.list = successorCurrent;
        var successorError = '';
        try { successorScheduler.schedule(); } catch (e) { successorError = e; }
        check(successorError === 'successor link' && linkGets === 1,
              'successor replay once');
        check(successorCurrent.queue === successor &&
              successorCurrent.state === STATE_SUSPENDED_RUNNABLE,
              'non-Null successor pending state');
        check(successorHandler.v2 === first && first.link === null,
              'successor/P owner moves');

        function workCase(withHead) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var incoming = new Packet(null, ID_HANDLER_A, KIND_WORK);
          var head = withHead ? new Packet(null, ID_HANDLER_A, KIND_WORK) : null;
          if (head !== null) handler.v1 = head;
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, incoming, handler);
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          return [scheduler, handler, current, head];
        }
        var empty = workCase(false);
        empty[0].schedule();
        check(empty[1].v1 !== null && empty[1].v1.link === null &&
              empty[1].v1.a1 === 0, 'WORK empty v1');
        check(empty[2].queue === null && empty[2].state === STATE_SUSPENDED &&
              empty[0].currentTcb === null, 'WORK empty suspend');

        var one = workCase(true), head = one[3];
        one[0].schedule();
        check(one[1].v1 === head && head.link !== null && head.link.link === null &&
              head.a1 === 0, 'WORK one-node append');
        check(one[2].queue === null && one[2].state === STATE_SUSPENDED &&
              one[0].currentId === ID_HANDLER_A && one[0].queueCount === 0,
              'WORK one-node suspend');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_suspend_replays_bounds_and_guards() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active Handler incoming replay: ' + message;
        }
        function oneIncoming(kind, v1, v2) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var packet = new Packet(null, ID_HANDLER_A, kind);
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, packet, handler);
          handler.v1 = v1;
          handler.v2 = v2;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          return [scheduler, handler, current, packet];
        }

        // Lists just beyond the unrolled subsets must replay Packet.addTo's full scan once.
        var d3 = new Packet(null, ID_WORKER, KIND_DEVICE);
        var d2 = new Packet(d3, ID_WORKER, KIND_DEVICE);
        var d1 = new Packet(d2, ID_WORKER, KIND_DEVICE);
        var longDevice = oneIncoming(KIND_DEVICE, null, d1);
        longDevice[0].schedule();
        check(d1.link === d2 && d2.link === d3 && d3.link === longDevice[3] &&
              longDevice[3].link === null, 'DEVICE length-three replay');

        var w2 = new Packet(null, ID_HANDLER_A, KIND_WORK);
        var w1 = new Packet(w2, ID_HANDLER_A, KIND_WORK);
        var longWork = oneIncoming(KIND_WORK, w1, null);
        longWork[0].schedule();
        check(w1.link === w2 && w2.link === longWork[3] &&
              longWork[3].link === null, 'WORK length-two replay');

        // An observable kind access declines before Active/Handler writes, then generic replay
        // performs the getter exactly once after TaskControlBlock.run's dequeue/state prefix.
        var kindCase = oneIncoming(KIND_DEVICE, null, null), kindGets = 0;
        Object.defineProperty(kindCase[3], 'kind', {
          get: function() {
            kindGets++;
            check(kindCase[2].queue === null && kindCase[2].state === STATE_RUNNING &&
                  kindCase[0].currentId === ID_HANDLER_A && kindCase[1].v2 === null,
                  'kind getter checkpoint');
            return KIND_DEVICE;
          }, configurable: true
        });
        kindCase[0].schedule();
        check(kindGets === 1 && kindCase[1].v2 === kindCase[3], 'kind getter once');

        var originalAdd = Packet.prototype.addTo, addHits = 0;
        Packet.prototype.addTo = function(queue) {
          addHits++;
          return originalAdd.call(this, queue);
        };
        var addCase = oneIncoming(KIND_DEVICE, null, null);
        addCase[0].schedule();
        Packet.prototype.addTo = originalAdd;
        check(addHits === 1 && addCase[1].v2 === addCase[3], 'addTo replacement once');

        var originalSuspend = Scheduler.prototype.suspendCurrent, suspendHits = 0;
        Scheduler.prototype.suspendCurrent = function() {
          suspendHits++;
          return originalSuspend.call(this);
        };
        var suspendCase = oneIncoming(KIND_DEVICE, null, null);
        suspendCase[0].schedule();
        Scheduler.prototype.suspendCurrent = originalSuspend;
        check(suspendHits === 1 && suspendCase[2].state === STATE_SUSPENDED,
              'suspend replacement once');

        var originalMark = TaskControlBlock.prototype.markAsSuspended, markHits = 0;
        TaskControlBlock.prototype.markAsSuspended = function() {
          markHits++;
          return originalMark.call(this);
        };
        var markCase = oneIncoming(KIND_DEVICE, null, null);
        markCase[0].schedule();
        TaskControlBlock.prototype.markAsSuspended = originalMark;
        check(markHits === 1 && markCase[1].v2 === markCase[3] &&
              markCase[2].state === STATE_SUSPENDED, 'mark replacement once');

        var schedulerCase = oneIncoming(KIND_DEVICE, null, null), schedulerGets = 0;
        var storedScheduler = schedulerCase[0];
        Object.defineProperty(schedulerCase[1], 'scheduler', {
          get: function() { schedulerGets++; return storedScheduler; }, configurable: true
        });
        schedulerCase[0].schedule();
        check(schedulerGets === 1 && schedulerCase[1].v2 === schedulerCase[3],
              'Handler.scheduler accessor once');

        // Non-numeric KIND_WORK forces generic loose-equality coercion exactly once.
        var oldKindWork = KIND_WORK, nameHits = 0;
        KIND_WORK = { valueOf: function() { nameHits++; return oldKindWork; } };
        var nameCase = oneIncoming(oldKindWork, null, null);
        nameCase[0].schedule();
        KIND_WORK = oldKindWork;
        check(nameHits === 1 && nameCase[1].v1 === nameCase[3] &&
              nameCase[1].v2 === null, 'KIND_WORK coercion replay');

        // P already present as the selected list head is a valid but uncommon source-order
        // alias. Ordinary addTo creates the self-link; the direct transaction must decline.
        var alias = oneIncoming(KIND_DEVICE, null, null);
        alias[1].v2 = alias[3];
        alias[0].schedule();
        check(alias[1].v2 === alias[3] && alias[3].link === alias[3] &&
              alias[2].state === STATE_SUSPENDED, 'P equals v2 replay');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_suspend_parity_case() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        var scheduler = new Scheduler();
        var handler = new HandlerTask(scheduler);
        var p3 = new Packet(null, ID_WORKER, KIND_DEVICE);
        var p2 = new Packet(p3, ID_WORKER, KIND_DEVICE);
        var p1 = new Packet(p2, ID_WORKER, KIND_DEVICE);
        var current = new TaskControlBlock(null, ID_HANDLER_A, 2, p1, handler);
        scheduler.blocks[ID_HANDLER_A] = current;
        scheduler.list = current;
        scheduler.schedule();
        var deviceState = current.state;
        var deviceQueueNull = current.queue === null;
        var deviceChainComplete = handler.v2.link.link.link === null;
        var deviceCurrentNull = scheduler.currentTcb === null;
        var wScheduler = new Scheduler();
        var wHandler = new HandlerTask(wScheduler);
        var oldWork = new Packet(null, ID_HANDLER_A, KIND_WORK);
        var newWork = new Packet(null, ID_HANDLER_A, KIND_WORK);
        var wCurrent = new TaskControlBlock(null, ID_HANDLER_A, 2, newWork, wHandler);
        wHandler.v1 = oldWork;
        wScheduler.blocks[ID_HANDLER_A] = wCurrent;
        wScheduler.list = wCurrent;
        wScheduler.schedule();
        var workState = wCurrent.state;
        [deviceState, deviceQueueNull, deviceChainComplete, deviceCurrentNull,
         workState, wHandler.v1 === oldWork,
         oldWork.link === newWork, newWork.link === null].join('|')
        "#,
    ]
    .join("\n");
    // This deliberately global construction retains the scheduler's runnable checkpoint; the
    // parent parity test executes the identical fixture with the stitched arm both on and off.
    assert_eq!(run_jit(&src), "2|true|true|true|2|true|true|true");
}

#[test]
fn jit_scheduler_active_handler_incoming_suspend_enabled_disabled_parity() {
    use std::process::Command;

    let executable = std::env::current_exe().expect("current test executable");
    for disabled in [false, true] {
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg("tests::jit_scheduler_active_handler_incoming_suspend_parity_case")
            .arg("--nocapture")
            .env_remove("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING_SUSPEND");
        if disabled {
            command.env("LUMEN_JIT_NO_SCHED_HANDLER_INCOMING_SUSPEND", "1");
        }
        let output = command.output().expect("run parity child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("running 1 test"),
            "Handler incoming suspend parity child disabled={disabled} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn jit_scheduler_handler_incoming_device_bridge_preserves_owners_and_replays_guards() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler incoming: ' + message;
        }
        function oneIncoming(link, count, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = count;
          work.a2[count] = payload;
          var packet = new Packet(link, ID_WORKER, KIND_DEVICE);
          var queued = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 1, queued, {
            run: function() { return null; }
          });
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, packet, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          handler.v1 = work;
          // Deliberately omit packet from the returned roots. A successful bridge must preserve
          // it through both skipped inline frames until the target queue receives the owner.
          return [scheduler, handler, current, target, work, queued];
        }

        var direct = oneIncoming(null, 1, 77);
        direct[0].schedule();
        var delivered = direct[3].queue.link;
        check(direct[0].queueCount === 1, 'direct queue count');
        check(delivered.a1 === 77 && delivered.id === ID_HANDLER_A,
              'direct packet fields');
        check(delivered.link === null && direct[4].a1 === 2 && direct[1].v2 === null,
              'direct Handler state');
        check(direct[2].queue === null && direct[2].state === STATE_SUSPENDED &&
              direct[0].currentId === ID_HANDLER_A && direct[0].currentTcb === null,
              'direct active state');

        // A successor is moved into current.queue by TaskControlBlock.run before Handler runs.
        // The bridge's pre-prefix Null-link guard must replay, after which both packets are
        // delivered once without losing the successor's last hidden owner.
        var successor = new Packet(null, ID_WORKER, KIND_DEVICE);
        successor.a2[0] = 91;
        var linked = oneIncoming(successor, 1, 81);
        linked[0].schedule();
        var first = linked[3].queue.link;
        check(linked[0].queueCount === 2 && first.a1 === 81, 'linked deliveries');
        check(first.link === successor && successor.link === null, 'linked owners');
        check(successor.a1 === linked[4].a2[2] && linked[4].a1 === 3,
              'linked cursor');
        check(linked[2].queue === null && linked[2].state === STATE_SUSPENDED,
              'linked suspension');

        // An observable kind read makes the generic Active fast path materialize and replay.
        // The getter must nevertheless observe only the source-ordered Active effects: currentId
        // and the queue/state prefix, but none of HandlerTask.run's writes.
        var early = oneIncoming(null, 1, 80);
        var earlyPacket = early[2].queue, kindThrows = 0;
        Object.defineProperty(earlyPacket, 'kind', {
          get: function() { kindThrows++; throw 'kind boom'; }, configurable: true
        });
        var kindError = '';
        try { early[0].schedule(); } catch (e) { kindError = e; }
        check(kindError === 'kind boom' && kindThrows === 1, 'early throw once');
        check(early[2].queue === null && early[2].state === STATE_RUNNING &&
              early[0].currentId === ID_HANDLER_A && early[0].currentTcb === early[2],
              'early active checkpoint');
        check(early[1].v2 === null && early[4].a1 === 1 &&
              early[0].queueCount === 0 && early[3].queue === early[5],
              'early Handler untouched');

        var originalAdd = Packet.prototype.addTo, addHits = 0;
        Packet.prototype.addTo = function(queue) {
          addHits++;
          return originalAdd.call(this, queue);
        };
        var methodCase = oneIncoming(null, 1, 82);
        methodCase[0].schedule();
        Packet.prototype.addTo = originalAdd;
        check(addHits === 2 && methodCase[3].queue.link.a1 === 82,
              'method replacement replay');

        var kindCase = oneIncoming(null, 1, 83);
        var kindPacket = kindCase[2].queue, kindGets = 0;
        Object.defineProperty(kindPacket, 'kind', {
          get: function() { kindGets++; return KIND_DEVICE; }, configurable: true
        });
        kindCase[0].schedule();
        check(kindGets === 1 && kindCase[3].queue.link === kindPacket,
              'kind getter replay');

        // Handler.v1 is observed after the incoming Packet.addTo side effects. A late throw must
        // therefore see the active-prefix and addTo effects exactly once, but no delivery writes.
        var throwCase = oneIncoming(null, 1, 84);
        var throwPacket = throwCase[2].queue, v1Gets = 0;
        Object.defineProperty(throwCase[1], 'v1', {
          get: function() { v1Gets++; throw 'v1 boom'; }, configurable: true
        });
        var error = '';
        try { throwCase[0].schedule(); } catch (e) { error = e; }
        check(error === 'v1 boom' && v1Gets === 1, 'late throw once');
        check(throwCase[2].queue === null && throwCase[2].state === STATE_RUNNING &&
              throwCase[0].currentId === ID_HANDLER_A &&
              throwCase[0].currentTcb === throwCase[2], 'late active effects');
        check(throwCase[1].v2 === throwPacket && throwPacket.link === null,
              'late addTo effects');
        check(throwCase[0].queueCount === 0 && throwCase[3].queue === throwCase[5] &&
              throwCase[5].link === null && throwCase[0].currentTcb === throwCase[2],
              'late target untouched');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_incoming_delivery_fuses_preempt_and_replays_late_guards() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler incoming delivery: ' + message;
        }
        function emptyTarget(targetPriority, currentPriority, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          work.a2[1] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var seen = { packet: null, handlerState: -1, wasCurrent: false };
          var current, target;
          var task = {
            run: function(value) {
              seen.packet = value;
              seen.handlerState = current.state;
              seen.wasCurrent = scheduler.currentTcb === target;
              return null;
            }
          };
          target = new TaskControlBlock(null, ID_WORKER, targetPriority, null, task);
          current = new TaskControlBlock(target, ID_HANDLER_A, currentPriority, packet, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          handler.v1 = work;
          return [scheduler, handler, current, target, work, packet, seen];
        }

        // The fused empty-target arm must publish exactly one packet owner and immediately
        // preempt when the target priority is higher. The Handler has not run its later suspend.
        var preempt = emptyTarget(3, 2, 71);
        preempt[0].schedule();
        check(preempt[6].packet === preempt[5] && preempt[6].wasCurrent,
              'preempt packet/current');
        check(preempt[6].handlerState === STATE_RUNNING,
              'preempt happens before Handler suspend');
        check(preempt[5].a1 === 71 && preempt[5].id === ID_HANDLER_A &&
              preempt[5].link === null, 'preempt packet fields');
        check(preempt[1].v2 === null && preempt[4].a1 === 2 &&
              preempt[0].queueCount === 1, 'preempt Handler writes');

        // The same empty target with a lower priority must decline the fused preempt guard and
        // replay pc59. Handler suspends before the linked target is selected, with no duplicates.
        var noPreempt = emptyTarget(1, 2, 72);
        noPreempt[0].schedule();
        check(noPreempt[6].packet === noPreempt[5] && noPreempt[6].wasCurrent,
              'nonpreempt packet/current');
        check(noPreempt[6].handlerState === STATE_SUSPENDED,
              'nonpreempt runs after Handler suspend');
        check(noPreempt[5].a1 === 72 && noPreempt[4].a1 === 2 &&
              noPreempt[0].queueCount === 1, 'nonpreempt writes once');

        function twoNodeTarget() {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          work.a2[1] = 73;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var tail = new Packet(null, ID_WORKER, KIND_DEVICE);
          var head = new Packet(tail, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 1, head, {
            run: function() { return null; }
          });
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, packet, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          handler.v1 = work;
          return [scheduler, handler, current, target, work, packet, head, tail];
        }

        // A two-node destination is intentionally outside the one-node transaction. Full replay
        // must run Packet.addTo's scan once and append rather than overwrite the existing tail.
        var scanned = twoNodeTarget();
        scanned[0].schedule();
        check(scanned[6].link === scanned[7] && scanned[7].link === scanned[5] &&
              scanned[5].link === null, 'two-node append order');
        check(scanned[0].queueCount === 1 && scanned[5].a1 === 73 &&
              scanned[4].a1 === 2 && scanned[1].v2 === null,
              'two-node replay writes once');

        function latePayloadThrow() {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          var payload = {}, hits = { value: 0 };
          Object.defineProperty(payload, '1', {
            get: function() { hits.value++; throw 'payload boom'; }, configurable: true
          });
          // Keep the Packet's warmed shape unchanged while making a2 non-packed. The incoming
          // prefix can fuse, but the later delivery guard must decline before committing.
          work.a2 = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var queued = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 1, queued, {
            run: function() { return null; }
          });
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, packet, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          handler.v1 = work;
          return [scheduler, handler, current, target, work, packet, queued, hits];
        }

        // Original execution has already published the incoming packet to v2 and advanced v2
        // back to Null when the payload getter throws. Replaying from pc161 would miss that order.
        var late = latePayloadThrow(), error = '';
        try { late[0].schedule(); } catch (e) { error = e; }
        check(error === 'payload boom' && late[7].value === 1, 'late throw once');
        check(late[1].v2 === null && late[5].link === null && late[5].a1 === 0 &&
              late[5].id === ID_WORKER, 'late prefix/delivery checkpoint');
        check(late[4].a1 === 1 && late[0].queueCount === 0 &&
              late[3].queue === late[6] && late[6].link === null,
              'late queue untouched');
        check(late[2].queue === null && late[2].state === STATE_RUNNING &&
              late[0].currentTcb === late[2], 'late active effects');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_active_handler_incoming_replays_changed_methods_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'active handler methods: ' + message;
        }
        function oneIncoming(payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          work.a2[1] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 3, null, {
            run: function() { throw 'held target ran'; }
          });
          target.state = STATE_SUSPENDED | STATE_HELD;
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, packet, handler);
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = current;
          scheduler.list = current;
          handler.v1 = work;
          return {
            scheduler: scheduler, handler: handler, work: work, packet: packet,
            current: current, target: target, payload: payload
          };
        }
        function checkComplete(one, label) {
          check(one.current.queue === null && one.current.state === STATE_RUNNING,
                label + ' source dequeue');
          check(one.scheduler.currentId === ID_HANDLER_A &&
                one.scheduler.currentTcb === null, label + ' scheduler state');
          check(one.handler.v2 === null && one.work.a1 === 2 &&
                one.packet.a1 === one.payload, label + ' Handler writes');
          check(one.scheduler.queueCount === 1 && one.packet.link === null &&
                one.packet.id === ID_HANDLER_A, label + ' queue prefix');
          check(one.target.queue === one.packet &&
                one.target.state === (STATE_SUSPENDED | STATE_HELD | STATE_RUNNABLE),
                label + ' target publication');
        }

        // TaskControlBlock.run dequeues and writes currentId before resolving Handler.run.
        var originalRun = HandlerTask.prototype.run;
        var runCase, runHits = 0, runPacket = null, runQueue = 1;
        var runState = -1, runId = -1, runV2 = 1;
        HandlerTask.prototype.run = function(packet) {
          runHits++;
          runPacket = packet;
          runQueue = runCase.current.queue;
          runState = runCase.current.state;
          runId = runCase.scheduler.currentId;
          runV2 = this.v2;
          return originalRun.call(this, packet);
        };
        runCase = oneIncoming(71);
        runCase.scheduler.schedule();
        HandlerTask.prototype.run = originalRun;
        check(runHits === 1 && runPacket === runCase.packet && runQueue === null &&
              runState === STATE_RUNNING && runId === ID_HANDLER_A && runV2 === null,
              'run entry once');
        checkComplete(runCase, 'run');

        // The first addTo is still before Handler.v2 is published. An empty target avoids a
        // second addTo inside checkPriorityAdd, making this call count exact.
        var originalAdd = Packet.prototype.addTo;
        var addCase, addHits = 0, addQueue = 1, addLink = 1, addCurrent = false;
        Packet.prototype.addTo = function(queue) {
          addHits++;
          addQueue = queue;
          addLink = this.link;
          addCurrent = addCase.scheduler.currentTcb === addCase.current &&
                       addCase.current.queue === null;
          return originalAdd.call(this, queue);
        };
        addCase = oneIncoming(72);
        addCase.scheduler.schedule();
        Packet.prototype.addTo = originalAdd;
        check(addHits === 1 && addQueue === null && addLink === null && addCurrent,
              'addTo entry once');
        checkComplete(addCase, 'addTo');

        // Scheduler.queue is resolved after all Handler delivery writes, but before its own
        // queueCount/link/id prefix.
        var originalQueue = Scheduler.prototype.queue;
        var queueCase, queueHits = 0, queueCountAtEntry = -1;
        var queueV2 = 1, queueCount = -1, queueId = -1, queueLink = 1;
        Scheduler.prototype.queue = function(packet) {
          queueHits++;
          queueCountAtEntry = this.queueCount;
          queueV2 = queueCase.handler.v2;
          queueCount = queueCase.work.a1;
          queueId = packet.id;
          queueLink = packet.link;
          return originalQueue.call(this, packet);
        };
        queueCase = oneIncoming(73);
        queueCase.scheduler.schedule();
        Scheduler.prototype.queue = originalQueue;
        check(queueHits === 1 && queueCountAtEntry === 0 && queueV2 === null &&
              queueCount === 2 && queueId === ID_WORKER && queueLink === null &&
              queueCase.packet.a1 === 73, 'queue entry once');
        checkComplete(queueCase, 'queue');

        // checkPriorityAdd sees Scheduler.queue's prefix and no target mutation yet.
        var originalCheck = TaskControlBlock.prototype.checkPriorityAdd;
        var checkCase, checkHits = 0, checkTask = null, checkQueue = 1;
        var checkState = -1, checkId = -1, checkCount = -1;
        TaskControlBlock.prototype.checkPriorityAdd = function(task, packet) {
          checkHits++;
          checkTask = task;
          checkQueue = this.queue;
          checkState = this.state;
          checkId = packet.id;
          checkCount = checkCase.scheduler.queueCount;
          return originalCheck.call(this, task, packet);
        };
        checkCase = oneIncoming(74);
        checkCase.scheduler.schedule();
        TaskControlBlock.prototype.checkPriorityAdd = originalCheck;
        check(checkHits === 1 && checkTask === checkCase.current && checkQueue === null &&
              checkState === (STATE_SUSPENDED | STATE_HELD) &&
              checkId === ID_HANDLER_A && checkCount === 1, 'check entry once');
        checkComplete(checkCase, 'check');

        // markAsRunnable is later still: target.queue owns the packet while state is unchanged.
        var originalMark = TaskControlBlock.prototype.markAsRunnable;
        var markCase, markHits = 0, markQueue = null, markState = -1;
        TaskControlBlock.prototype.markAsRunnable = function() {
          markHits++;
          markQueue = this.queue;
          markState = this.state;
          return originalMark.call(this);
        };
        markCase = oneIncoming(75);
        markCase.scheduler.schedule();
        TaskControlBlock.prototype.markAsRunnable = originalMark;
        check(markHits === 1 && markQueue === markCase.packet &&
              markState === (STATE_SUSPENDED | STATE_HELD), 'mark entry once');
        checkComplete(markCase, 'mark');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_v2_delivery_preserves_aliases_and_owner_moves() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler v2 owners: ' + message;
        }
        function oneDelivery(count, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = count;
          work.a2[count] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var queued = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 1, queued, {
            run: function() { return null; }
          });
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, null, handler);
          current.state = STATE_RUNNING;
          handler.v1 = work;
          handler.v2 = packet;
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          return [scheduler, handler, current, target, packet, work, queued];
        }

        var direct = oneDelivery(1, 77);
        direct[0].schedule();
        check(direct[0].queueCount === 1, 'direct count');
        check(direct[1].v2 === null && direct[4].link === null, 'direct source');
        check(direct[4].a1 === 77 && direct[5].a1 === 2, 'direct numerics');
        check(direct[4].id === ID_HANDLER_A, 'direct id');
        check(direct[3].queue === direct[6] && direct[6].link === direct[4],
              'direct append');
        check(direct[2].state === STATE_SUSPENDED && direct[0].currentTcb === null,
              'direct suspension');

        // Valid aliases currently replay at the transaction head. These assertions pin the
        // source-order behavior so a future alias-specialized commit cannot silently drift.
        var queueIsPacket = oneDelivery(3, 70);
        var successor = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        successor.a2[0] = 91;
        queueIsPacket[4].link = successor;
        queueIsPacket[3].queue = queueIsPacket[4];
        queueIsPacket[0].schedule();
        check(queueIsPacket[1].v2 === successor, 'Q=P successor');
        check(queueIsPacket[4].link === queueIsPacket[4], 'Q=P self append');
        check(queueIsPacket[3].queue === queueIsPacket[4], 'Q=P queue');

        var workIsPacket = oneDelivery(1, 88);
        workIsPacket[1].v1 = workIsPacket[4];
        workIsPacket[4].a1 = 1;
        workIsPacket[4].a2[1] = 88;
        workIsPacket[0].schedule();
        check(workIsPacket[4].a1 === 2, 'W=P ordered a1 writes');
        check(workIsPacket[6].link === workIsPacket[4], 'W=P append');

        var queueIsWork = oneDelivery(1, 66);
        queueIsWork[3].queue = queueIsWork[5];
        queueIsWork[0].schedule();
        check(queueIsWork[5].link === queueIsWork[4], 'Q=W append');
        check(queueIsWork[5].a1 === 2 && queueIsWork[4].a1 === 66,
              'Q=W distinct fields');

        var linkIsQueue = oneDelivery(3, 65);
        linkIsQueue[4].link = linkIsQueue[6];
        linkIsQueue[0].schedule();
        check(linkIsQueue[1].v2 === linkIsQueue[6], 'L=Q transfer');
        check(linkIsQueue[6].link === linkIsQueue[4], 'L=Q append');

        var selfSource = oneDelivery(3, 64);
        selfSource[4].link = selfSource[4];
        selfSource[0].schedule();
        check(selfSource[1].v2 === selfSource[4], 'L=P transfer');
        check(selfSource[4].link === null && selfSource[6].link === selfSource[4],
              'L=P queue clear');

        var lastOwner = oneDelivery(3, 63);
        lastOwner[4].link = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        lastOwner[4].link.a2[0] = 92;
        lastOwner[0].schedule();
        check(lastOwner[1].v2.a2[0] === 92, 'last-owner successor');
        check(lastOwner[4].link === null && lastOwner[6].link === lastOwner[4],
              'last-owner packet move');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_v2_delivery_guards_effect_order_and_live_values() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler v2 guards: ' + message;
        }
        function oneDelivery(count, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = count;
          work.a2[count] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var queued = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 1, queued, {
            run: function() { return null; }
          });
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, null, handler);
          current.state = STATE_RUNNING;
          handler.v1 = work;
          handler.v2 = packet;
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          return [scheduler, handler, current, target, packet, work, queued];
        }

        // The region must use the count local captured before this getter changes Handler.v1.
        var changed = oneDelivery(0, 10);
        var oldWork = changed[5];
        var newWork = new Packet(null, ID_HANDLER_A, KIND_WORK);
        newWork.a1 = 0;
        newWork.a2[1] = 81;
        Object.defineProperty(oldWork, 'a1', {
          get: function() { changed[1].v1 = newWork; return 1; }, configurable: true
        });
        changed[0].schedule();
        check(changed[4].a1 === 81 && newWork.a1 === 2, 'captured count');
        check(changed[6].link === changed[4], 'changed work append');

        var hole = oneDelivery(1, 11);
        delete hole[5].a2[1];
        Array.prototype[1] = 73;
        hole[0].schedule();
        delete Array.prototype[1];
        check(hole[4].a1 === 73, 'inherited payload hole');

        var element = oneDelivery(1, 12), elementGets = 0;
        Object.defineProperty(element[5].a2, '1', {
          get: function() { elementGets++; return 74; }, configurable: true
        });
        element[0].schedule();
        check(elementGets === 1 && element[4].a1 === 74, 'payload getter');

        var payloadObject = { marker: 75 };
        var objectPayload = oneDelivery(1, payloadObject);
        objectPayload[0].schedule();
        check(objectPayload[4].a1 === payloadObject, 'object payload ownership');

        var originalAdd = Packet.prototype.addTo, addHits = 0;
        Packet.prototype.addTo = function(queue) {
          addHits++;
          return originalAdd.call(this, queue);
        };
        var addCase = oneDelivery(1, 76);
        addCase[0].schedule();
        Packet.prototype.addTo = originalAdd;
        check(addHits === 1 && addCase[6].link === addCase[4], 'addTo replacement');

        var linkCase = oneDelivery(1, 77), storedLink = null;
        var linkGets = 0, linkSets = 0;
        Object.defineProperty(linkCase[4], 'link', {
          get: function() { linkGets++; return storedLink; },
          set: function(value) { linkSets++; storedLink = value; }, configurable: true
        });
        linkCase[0].schedule();
        check(linkGets > 0 && linkSets > 0 && storedLink === null, 'packet link accessor');
        check(linkCase[6].link === linkCase[4], 'packet link accessor append');

        var twoNode = oneDelivery(1, 78);
        var tail = new Packet(null, ID_WORKER, KIND_DEVICE);
        twoNode[6].link = tail;
        twoNode[0].schedule();
        check(twoNode[6].link === tail && tail.link === twoNode[4], 'two-node replay');

        var undefinedLink = oneDelivery(1, 79);
        undefinedLink[6].link = undefined;
        undefinedLink[0].schedule();
        check(undefinedLink[6].link === undefinedLink[4], 'undefined queued link');
        var ddaLink = oneDelivery(1, 80);
        ddaLink[6].link = $262.IsHTMLDDA;
        ddaLink[0].schedule();
        check(ddaLink[6].link === ddaLink[4], 'HTMLDDA queued link');

        // Missing target returns before Scheduler.queue's own writes, but Handler's three writes
        // have already happened and must be replayed exactly once.
        var missing = oneDelivery(1, 82);
        var missingSuccessor = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        missing[4].link = missingSuccessor;
        missing[0].blocks[ID_WORKER] = null;
        missing[0].schedule();
        check(missing[1].v2 === missingSuccessor, 'missing source transfer');
        check(missing[4].a1 === 82 && missing[5].a1 === 2, 'missing Handler writes');
        check(missing[0].queueCount === 0, 'missing queue count');
        check(missing[4].link === missingSuccessor && missing[4].id === ID_WORKER,
              'missing packet untouched');

        // An id setter throws after queueCount and packet.link, but before target mutation.
        var idThrow = oneDelivery(1, 83), idValue = ID_WORKER, idSets = 0;
        Object.defineProperty(idThrow[4], 'id', {
          get: function() { return idValue; },
          set: function(value) { idSets++; throw 'id boom'; }, configurable: true
        });
        var idError = '';
        try { idThrow[0].schedule(); } catch (e) { idError = e; }
        check(idError === 'id boom' && idSets === 1, 'id throw once');
        check(idThrow[1].v2 === null && idThrow[4].a1 === 83 && idThrow[5].a1 === 2,
              'id throw Handler effects');
        check(idThrow[0].queueCount === 1 && idThrow[4].link === null,
              'id throw queue prefix');
        check(idThrow[6].link === null, 'id throw target untouched');

        // The tail setter throws after every queue prefix write and before target.queue's no-op.
        var tailThrow = oneDelivery(1, 84), tailValue = null, tailSets = 0;
        Object.defineProperty(tailThrow[6], 'link', {
          get: function() { return tailValue; },
          set: function(value) { tailSets++; throw 'tail boom'; }, configurable: true
        });
        var tailError = '';
        try { tailThrow[0].schedule(); } catch (e) { tailError = e; }
        check(tailError === 'tail boom' && tailSets === 1, 'tail throw once');
        check(tailThrow[1].v2 === null && tailThrow[4].a1 === 84 && tailThrow[5].a1 === 2,
              'tail throw Handler effects');
        check(tailThrow[0].queueCount === 1 && tailThrow[4].link === null,
              'tail throw queue prefix');
        check(tailThrow[4].id === ID_HANDLER_A && tailThrow[3].queue === tailThrow[6],
              'tail throw target state');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_v2_empty_preempt_preserves_effects_and_owners() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler v2 empty: ' + message;
        }
        function emptyDelivery(targetState, targetPriority, currentPriority, payload) {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          work.a2[1] = payload;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var observed = { calls: 0, packet: null };
          var target = new TaskControlBlock(null, ID_WORKER, targetPriority, null, {
            run: function(value) {
              observed.calls++;
              observed.packet = value;
              return null;
            }
          });
          target.state = targetState;
          var current = new TaskControlBlock(null, ID_HANDLER_A, currentPriority, null, handler);
          current.state = STATE_RUNNING;
          handler.v1 = work;
          handler.v2 = packet;
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          return [scheduler, handler, current, target, packet, work, observed];
        }

        // A held higher-priority target exposes the transaction after preemption without
        // consuming the packet on the following scheduler iteration.
        var held = emptyDelivery(STATE_SUSPENDED | STATE_HELD, 3, 2, 91);
        var successor = new Packet(null, ID_DEVICE_A, KIND_DEVICE);
        successor.a2[0] = 92;
        held[4].link = successor;
        held[0].schedule();
        check(held[0].queueCount === 1, 'held queue count');
        check(held[1].v2 === successor && held[4].link === null, 'held owner moves');
        check(held[4].a1 === 91 && held[5].a1 === 2, 'held numeric writes');
        check(held[4].id === ID_HANDLER_A, 'held packet id');
        check(held[3].queue === held[4] && held[3].state === 7, 'held target publish');
        check(held[6].calls === 0 && held[0].currentTcb === null, 'held preemption');

        // The source successor may be the old current TCB. Its link owner moves to Handler.v2
        // before Scheduler.current releases its separate owner.
        var currentLink = emptyDelivery(STATE_SUSPENDED | STATE_HELD, 4, 2, 93);
        currentLink[4].link = currentLink[2];
        currentLink[0].schedule();
        check(currentLink[1].v2 === currentLink[2], 'L=C survives current release');
        check(currentLink[1].v2.priority === 2 && currentLink[4].link === null,
              'L=C remains live');
        check(currentLink[3].queue === currentLink[4], 'L=C packet move');

        // A suspended target immediately consumes the newly queued packet after preemption.
        var consume = emptyDelivery(STATE_SUSPENDED, 3, 2, 94);
        consume[0].schedule();
        check(consume[6].calls === 1 && consume[6].packet === consume[4],
              'suspended target consumes identity');
        check(consume[3].queue === null && consume[3].state === STATE_RUNNING,
              'suspended target state');
        check(consume[1].v2 === null && consume[4].a1 === 94, 'consume source writes');

        // The runnable bit is read from the live global, not baked into generated code.
        var oldRunnable = STATE_RUNNABLE;
        STATE_RUNNABLE = 8;
        var liveFlag = emptyDelivery(STATE_HELD, 5, 2, 95);
        liveFlag[0].schedule();
        STATE_RUNNABLE = oldRunnable;
        check(liveFlag[3].queue === liveFlag[4] && liveFlag[3].state === 12,
              'live runnable flag');

        // Non-preemption deliberately replays, but all ordinary effects must still occur once.
        var noPreempt = emptyDelivery(STATE_SUSPENDED | STATE_HELD, 1, 2, 96);
        noPreempt[0].schedule();
        check(noPreempt[0].queueCount === 1 && noPreempt[3].queue === noPreempt[4],
              'nonpreempt queue');
        check(noPreempt[3].state === 7 && noPreempt[4].a1 === 96,
              'nonpreempt state and payload');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_scheduler_handler_v2_empty_preempt_replays_observable_guards_once() {
    let src = [
        include_str!("../../../v8-v7/base.js"),
        include_str!("../../../v8-v7/richards.js"),
        r#"
        for (var i = 0; i < 110; i++) runRichards();
        function check(value, message) {
          if (!value) throw 'handler v2 empty guards: ' + message;
        }
        function emptyDelivery() {
          var scheduler = new Scheduler();
          var handler = new HandlerTask(scheduler);
          var work = new Packet(null, ID_HANDLER_A, KIND_WORK);
          work.a1 = 1;
          work.a2[1] = 77;
          var packet = new Packet(null, ID_WORKER, KIND_DEVICE);
          var target = new TaskControlBlock(null, ID_WORKER, 3, null, {
            run: function() { return null; }
          });
          target.state = STATE_SUSPENDED | STATE_HELD;
          var current = new TaskControlBlock(null, ID_HANDLER_A, 2, null, handler);
          current.state = STATE_RUNNING;
          handler.v1 = work;
          handler.v2 = packet;
          scheduler.blocks[ID_WORKER] = target;
          scheduler.blocks[ID_HANDLER_A] = null;
          scheduler.list = current;
          return [scheduler, handler, current, target, packet, work];
        }

        var originalMark = TaskControlBlock.prototype.markAsRunnable, markCalls = 0;
        TaskControlBlock.prototype.markAsRunnable = function() {
          markCalls++;
          return originalMark.call(this);
        };
        var markCase = emptyDelivery();
        markCase[0].schedule();
        TaskControlBlock.prototype.markAsRunnable = originalMark;
        check(markCalls === 1 && markCase[3].queue === markCase[4],
              'mark replacement once');
        check(markCase[3].state === 7, 'mark replacement state');

        var stateCase = emptyDelivery(), stateValue = stateCase[3].state;
        var stateGets = 0, stateSets = 0;
        Object.defineProperty(stateCase[3], 'state', {
          get: function() { stateGets++; return stateValue; },
          set: function(value) { stateSets++; stateValue = value; }, configurable: true
        });
        stateCase[0].schedule();
        check(stateGets > 0 && stateSets === 1 && stateValue === 7,
              'state accessor effects');
        check(stateCase[3].queue === stateCase[4], 'state accessor packet');

        var targetPriority = emptyDelivery(), targetPriorityGets = 0;
        Object.defineProperty(targetPriority[3], 'priority', {
          get: function() { targetPriorityGets++; return 3; }, configurable: true
        });
        targetPriority[0].schedule();
        check(targetPriorityGets === 1 && targetPriority[3].queue === targetPriority[4],
              'target priority getter once');

        // The throwing priority read happens after Handler, queue-prefix, target.queue, and
        // markAsRunnable effects in ordinary source order, but before Scheduler.current changes.
        var throwing = emptyDelivery(), currentPriorityGets = 0;
        Object.defineProperty(throwing[2], 'priority', {
          get: function() { currentPriorityGets++; throw 'priority boom'; }, configurable: true
        });
        var error = '';
        try { throwing[0].schedule(); } catch (e) { error = e; }
        check(error === 'priority boom' && currentPriorityGets === 1, 'priority throw once');
        check(throwing[1].v2 === null && throwing[4].a1 === 77 && throwing[5].a1 === 2,
              'priority throw Handler effects');
        check(throwing[0].queueCount === 1 && throwing[4].link === null &&
              throwing[4].id === ID_HANDLER_A, 'priority throw queue prefix');
        check(throwing[3].queue === throwing[4] && throwing[3].state === 7,
              'priority throw target effects');
        check(throwing[0].currentTcb === throwing[2], 'priority throw current untouched');
        'ok'
        "#,
    ]
    .join("\n");
    assert_eq!(run_jit(&src), "ok");
}

#[test]
fn jit_reads_packed_dense_values_without_losing_identity() {
    assert_eq!(
        run_jit(
            "var obj={x:7}, sym=Symbol('s');
             var a=[obj,'text',true,null,undefined,sym,13.5];
             function local(a,i){return a[i];}
             function expr(a,i){return (i<99 ? a : [])[i];}
             for(var n=0;n<1000;n++) {
               local(a,n%7); expr(a,n%7);
             }
             var hole=[1,,3];
             Array.prototype[1]='inherited';
             var out=[local(a,0)===obj,local(a,1),local(a,2),local(a,3)===null,
                      local(a,4)===undefined,local(a,5)===sym,local(a,6),local(hole,1)];
             delete Array.prototype[1];
             out.join('|')"
        ),
        "true|text|true|true|true|true|13.5|inherited"
    );
}

#[test]
fn jit_writes_packed_dense_values_without_losing_ownership() {
    assert_eq!(
        run_jit(
            "var obj={x:7}, sym=Symbol('s'), a=[0,1,2,3,4,5,6,7];
             function drop(a,i,v){a[i]=v;}
             function keep(a,i,v){return a[i]=v;}
             function expr(a,i,v){return (i<99?a:[])[i]=v;}
             function read(a,i){return a[i];}
             for(var n=0;n<1000;n++) {
               drop(a,n&7,n); keep(a,n&7,n+1); expr(a,n&7,n+2); read(a,n&7);
             }
             drop(a,0,obj); drop(a,1,'text'); drop(a,2,true); drop(a,3,null);
             drop(a,4,undefined); drop(a,5,sym); drop(a,6,13.5);
             var kept=keep(a,7,obj); drop(a,6,2); var mirrored=read(a,6)===2;
             var expressed=expr(a,6,sym);
             a[1]=a; drop(a,1,obj);
             drop(a,2,9n);
             Object.defineProperty(a,'3',{value:33,writable:false}); drop(a,3,44);
             var seen=0; Object.defineProperty(a,'4',{set(v){seen=v}}); drop(a,4,55);
             var b=new ArrayBuffer(8), d=new DataView(b);
             d.setUint32(0,0x7ff90000); d.setUint32(4,1); drop(a,7,d.getFloat64(0));
              [a[0]===obj,a[1]===obj,a[2]===9n,a[3],seen,a[5]===sym,a[6]===sym,
              kept===obj,expressed===sym,mirrored,Number.isNaN(a[7])].join('|')"
        ),
        "true|true|true|33|55|true|true|true|true|true|true"
    );
}

#[test]
fn jit_compact_warmed_property_probes_deopt_cleanly() {
    assert_eq!(
        run_jit(
            "function read(o) { return o.x; }
             var a = { x: 1 };
             var otherShape = { pad: 0, x: 2 };
             for (var i = 0; i < 300; i++) read(a);
             var alternate = read(otherShape);
             Object.defineProperty(a, 'x', {
               get: function () { return 7; }, configurable: true
             });
             var accessor = read(a);
             var p1 = { x: 4 }, p2 = { x: 9 };
             var child = Object.create(p1);
             for (var i = 0; i < 300; i++) read(child);
             Object.setPrototypeOf(child, p2);
             alternate + ':' + accessor + ':' + read(child)"
        ),
        "2:7:9"
    );
}

#[test]
fn jit_numeric_property_chains_guard_live_values_and_shapes() {
    assert_eq!(
        run_jit(
            "function below(o, n) { return o.x < n; }
             function same(n) { return this.x === n; }
             var a = { x: 3 }, holder = { x: 5 }, child = Object.create(holder);
             var m = { x: 7, same: same };
             for (var i = 0; i < 500; i++) {
               below(a, 4); below(child, 6); m.same(7);
             }
             var warm = below(a, 4) + ':' + below(child, 6) + ':' + m.same(7);
             a.x = '9';
             var typeChange = below(a, 10);
             var other = { pad: 0, x: 2 };
             var shapeChange = below(other, 3);
             Object.defineProperty(holder, 'x', { get: function () { return 11; } });
             var accessor = below(child, 12);
             m.x = 8;
             var thisMutation = m.same(8);
             var b = new ArrayBuffer(8), d = new DataView(b);
             d.setUint32(0, 0x7ff80000); d.setUint32(4, 1);
             var nan = { x: d.getFloat64(0) };
             var nanResult = below(nan, 99);
             var FLAG = 2;
             function mark() { this.state = this.state | FLAG; return this.state; }
             var state = { state: 1, mark: mark };
             for (var i = 0; i < 500; i++) { state.state = 1; state.mark(); }
             var stored = state.state;
             state.state = '1'; var typeStore = state.mark();
             var seen = 0;
             Object.defineProperty(state, 'state', {
               get: function () { return 1; },
               set: function (v) { seen = v; }, configurable: true
             });
             state.mark();
             [warm,typeChange,shapeChange,accessor,thisMutation,nanResult,
              stored,typeStore,seen].join('|')"
        ),
        "true:true:true|true|true|true|true|false|3|3|3"
    );
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
    // Function.prototype.caller/arguments: the getter reflects the call stack for an ordinary
    // sloppy function (null while inactive — legacy web compat) and throws for strict ones; the
    // setter always throws.
    assert_eq!(run("function f(){}; String(f.caller)"), "null");
    assert_eq!(run("function f(){}; String(f.arguments)"), "null");
    assert_eq!(
        throws("function f(){}; 'use strict'; f.caller = 1"),
        "TypeError"
    );
    assert_eq!(
        throws("'use strict'; function f(){ return f.caller; }; f()"),
        "TypeError"
    );
    assert_eq!(
        throws("var f=(function(){'use strict';return function g(){}})(); f.arguments"),
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
/// A test-only native that flips the interpreter's tail-call-eligibility flag on. It stands in
/// for the promise-reaction machinery, which can leave `tco_ok == true` ambient while a coroutine
/// body is running.
fn leak_tco(
    i: &mut crate::interpreter::Interp,
    _this: crate::value::Value,
    _args: &[crate::value::Value],
) -> Result<crate::value::Value, crate::value::Value> {
    i.tco_ok = true;
    Ok(crate::value::Value::Undefined)
}

#[test]
fn async_tail_return_survives_leaked_tco() {
    // Regression: a coroutine (async/generator) body runs outside `Interp::call`'s tail-call
    // trampoline, so a top-level `return f(...)` there must NOT be treated as a proper tail call —
    // it would be parked as a pending tail call that nothing runs, resolving the async function to
    // `undefined`. `tco_ok` is ambient state a promise reaction can leave set to `true`, so the
    // body forces it off before each statement. Here `__leakTco()` reproduces that leaked state
    // after an `await`, and the following tail-call `return` must still yield its real value.
    let mut e = Engine::new();
    let global = e.interp.global.clone();
    e.interp.def_method(&global, "__leakTco", 0, leak_tco);
    e.eval(
        "function id(x){ return x; }\n\
         var out = 'unset';\n\
         (async () => { await null; __leakTco(); return id('kept'); })().then((v) => { out = v; });",
        false,
    )
    .expect("parse");
    assert_eq!(
        match e.eval("out", false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        },
        "kept"
    );
}

#[test]
fn bytecode_property_inline_cache() {
    // Exercise the GetProp/SetProp inline caches under the bytecode tier: repeated access at one
    // site across same- and different-shaped objects (slot revalidation), accessor shadowing (must
    // run the getter, not read a raw slot), own-shadows-proto + delete falling back to the proto,
    // and writes through the SetProp cache.
    let mut e = Engine::new();
    e.interp.tier = crate::bytecode::Tier::Bytecode;
    e.interp.tier_threshold = 0; // compile on first call so the caches are exercised
    let src = r#"
      function readXY(o){ return o.x + "," + o.y; }
      let a = "";
      for (let i=0;i<5;i++) a += readXY({ x: i, y: i*2 }) + ";"; // monomorphic hits
      a += readXY({ z: 9, y: 100, x: 200 }) + ";";                // different slots -> revalidate
      a += readXY({ x: 1, get y(){ return 42; } }) + ";";          // accessor -> run getter
      const proto = { x: "PX" };
      const obj = Object.create(proto); obj.x = "OWN";
      function readX(o){ return o.x; }
      let b = readX(obj); delete obj.x; b += "," + readX(obj);      // own, then proto after delete
      function bump(o){ o.n = o.n + 1; return o.n; }
      const c1 = { n: 10 }, c2 = { n: 20 };
      let w = bump(c1) + "," + bump(c2) + "," + bump(c1);          // SetProp cache across objects
      a + "|" + b + "|" + w;
    "#;
    let got = match e.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    };
    assert_eq!(got, "0,0;1,2;2,4;3,6;4,8;200,100;1,42;|OWN,PX|11,21,12");
}

#[test]
fn compiled_parameterless_arguments_object() {
    // Parameterless variadic helpers can keep `arguments` in a VM/JIT slot. The object is still
    // fresh per call, exposes the real callee in sloppy code, poisons it in strict code, and
    // carries all surplus arguments even though the compiled function has zero parameter slots.
    let src = r#"
      function collect() {
        return arguments.length + ":" + arguments[0] + ":" + arguments[2] + ":" +
          (arguments.callee === collect) + ":" + Array.prototype.join.call(arguments, ",");
      }
      function fresh() { return arguments; }
      function strictArgs() { "use strict"; try { return arguments.callee; } catch (e) { return e.constructor.name; } }
      collect("a", "b", "c") + "|" + (fresh() !== fresh()) + "|" + strictArgs();
    "#;
    for tier in [crate::bytecode::Tier::Bytecode, crate::bytecode::Tier::Jit] {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        let got = match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        };
        assert_eq!(got, "3:a:c:true:a,b,c|true|TypeError");
    }
}

#[test]
fn jit_function_apply_forwards_dense_arguments() {
    // The ARM64 call intrinsic moves an unmapped, dense arguments list directly into a compiled
    // target. A deleted entry must leave that path and preserve the inherited indexed getter.
    assert_eq!(
        run_jit(
            "function sum(a,b,c){ return this.bias+a+b+c; }
             var recv={bias:10};
             function forward(){ return sum.apply(recv, arguments); }
             var n=0;
             for(var i=0;i<600;i++) n=forward(i,2,3);
             Object.defineProperty(Object.prototype,'1',
               {get:function(){return 20}, configurable:true});
             function holey(){ delete arguments[1]; return sum.apply(recv,arguments); }
             var h=holey(1,2,3);
             delete Object.prototype['1'];
             n+':'+h"
        ),
        "614:34"
    );
}

#[test]
fn jit_construct_arguments_apply_forwarder_preserves_live_guards() {
    assert_eq!(
        run_jit(
            "function Wrapper(){this.initialize.apply(this,arguments);}
             function init(a,b){this.sum=a+b;this.argc=arguments.length;return {replace:true};}
             Wrapper.prototype.initialize=init;
             var value;
             for(var i=0;i<600;i++) value=new Wrapper(i,2);
             var out=[value.sum,value.argc,value.replace===undefined];

             var overrideCalls=0;
             init.apply=function(recv,list){
               overrideCalls++;
               recv.sum=list[0]*list[1];
               recv.argc=list.length;
               return {replace:true};
             };
             value=new Wrapper(3,4);
             out.push(value.sum,value.argc,overrideCalls,value.replace===undefined);
             delete init.apply;

             var applyGets=0;
             Object.defineProperty(init,'apply',{
               configurable:true,
               get:function(){applyGets++;return Function.prototype.apply;}
             });
             value=new Wrapper(5,6);
             out.push(value.sum,applyGets);
             delete init.apply;

             var initializeGets=0;
             Object.defineProperty(Wrapper.prototype,'initialize',{
               configurable:true,
               get:function(){initializeGets++;return init;}
             });
             value=new Wrapper(7,8);
             out.push(value.sum,initializeGets);

             Object.defineProperty(Wrapper.prototype,'initialize',{
               configurable:true,
               get:function(){initializeGets++;throw new Error('getter');}
             });
             try{new Wrapper(1,2);}catch(e){out.push(e.message,initializeGets);}
             out.join(':')"
        ),
        "601:2:true:12:2:1:true:11:1:15:1:getter:2"
    );
}

#[test]
fn compiled_typeof_unresolved_name() {
    for tier in [crate::bytecode::Tier::Bytecode, crate::bytecode::Tier::Jit] {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        let got = match e
            .eval(
                "function f(){ return typeof __missing_compiled_name; } f()",
                false,
            )
            .expect("parse")
        {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        };
        assert_eq!(got, "undefined");
    }
}

#[test]
fn compiled_update_free_name() {
    let src = r#"
      var g = 4;
      function outer() {
        let x = 7;
        return function bump() {
          var old = x++;
          ++g;
          g += x;
          var scaled = (g *= 2);
          return old + ":" + x + ":" + g + ":" + scaled;
        };
      }
      var bump = outer();
      bump() + "|" + bump();
    "#;
    for tier in [crate::bytecode::Tier::Bytecode, crate::bytecode::Tier::Jit] {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        let got = match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        };
        assert_eq!(got, "7:8:26:26|8:9:72:72");
    }
}

#[test]
fn compiled_regexp_literal_is_fresh() {
    let src = r#"
      function make() { return /a+/gi; }
      var a = make(), b = make();
      a.lastIndex = 7;
      (a !== b) + ":" + b.lastIndex + ":" + b.source + ":" + b.flags;
    "#;
    let stmts = crate::parser::parse_script(src, false).ok().expect("parse");
    let func = stmts
        .iter()
        .find_map(|s| match s {
            crate::ast::Stmt::FuncDecl(f) => Some(f.clone()),
            _ => None,
        })
        .expect("function declaration");
    assert!(crate::bytecode::compile(&func).is_some());
    for tier in [crate::bytecode::Tier::Bytecode, crate::bytecode::Tier::Jit] {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        let got = match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        };
        assert_eq!(got, "true:0:a+:gi");
    }
}

#[test]
fn bytecode_compiles_labelled_loops() {
    // Labelled loops used to bail out of the compiler (falling back to the interpreter). They now
    // compile to the fast tier: assert `compile` actually produces a chunk rather than `None`.
    fn compiles(src: &str) -> bool {
        let stmts = crate::parser::parse_script(src, false).ok().expect("parse");
        let func = stmts
            .iter()
            .find_map(|s| match s {
                crate::ast::Stmt::FuncDecl(f) => Some(f.clone()),
                _ => None,
            })
            .expect("a function declaration");
        crate::bytecode::compile(&func).is_some()
    }
    assert!(compiles(
        "function f(){ var i=0; a: while(i<3){ i++; continue a; } }"
    ));
    assert!(compiles("function f(){ a: do { break a; } while(false); }"));
    assert!(compiles("function f(){ a: for(;;){ continue a; } }"));
    assert!(compiles(
        "function f(){ var r=0; a: b: for(var i=0;i<2;i++){ continue a; } }"
    ));
    assert!(compiles(
        "function f(){ outer: for(var i=0;i<2;i++){ for(var j=0;j<2;j++){ continue outer; } } }"
    ));
    // A label on a non-loop statement stays outside the compiled subset (bails to the interpreter).
    assert!(!compiles("function f(){ a: { break a; } }"));
}

#[test]
fn bytecode_labelled_loops_match_interp() {
    // The compiled labelled-loop behavior must match the tree-walker exactly. Run each snippet on
    // both tiers (threshold 0 forces immediate compilation) and require identical results.
    fn on_tier(src: &str, tier: crate::bytecode::Tier) -> String {
        let mut e = Engine::new();
        e.interp.tier = tier;
        e.interp.tier_threshold = 0;
        match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    for src in [
        "function f(){ var i=0; a: while(i<3){ i++; continue a; } return i; } f()",
        "function f(){ var i=0; a: do { i++; continue a; } while(i<3); return i; } f()",
        "function f(){ var n=0; a: while(n<5){ n++; if(n===3) break a; } return n; } f()",
        "function f(){ var r=0; a: b: for(var i=0;i<4;i++){ if(i===2) continue a; r+=i; } return r; } f()",
        "function f(){ var s=0; outer: for(var i=0;i<3;i++){ for(var j=0;j<3;j++){ if(j===1) continue outer; s+=10*i+j; } } return s; } f()",
        "function f(){ var s=''; a: for(var i=0;i<3;i++){ for(var j=0;j<3;j++){ if(j===1) break a; s+=i+''+j; } } return s; } f()",
    ] {
        let interp = on_tier(src, crate::bytecode::Tier::Interp);
        let bytecode = on_tier(src, crate::bytecode::Tier::Bytecode);
        assert_eq!(interp, bytecode, "tier mismatch for: {src}");
    }
}

#[test]
fn bytecode_async_vm() {
    // Async bodies compile to the bytecode VM and suspend at `await` without an OS-thread
    // coroutine. Checks the awaited value flows back, `await` in a loop accumulates, the return
    // value is delivered, and `await` still yields a microtask tick (ordering "123", not "132").
    let mut e = Engine::new();
    e.interp.tier = crate::bytecode::Tier::Bytecode;
    e.interp.tier_threshold = 0; // compile every function so the VM async path is taken
    let src = r#"
      var out = "";
      async function add(a, b){ return a + await Promise.resolve(b); }
      async function chain(){ let s = 0; for (let i=0;i<4;i++) s += await add(i, 10); return s; }
      const order = [];
      async function stepper(){ order.push(1); await 0; order.push(3); }
      async function main(){
        const c = await chain();          // 10+11+12+13 = 46
        const p = stepper(); order.push(2); await p;
        out = c + "|" + order.join("");
      }
      main();
    "#;
    e.eval(src, false).expect("parse");
    let got = match e.eval("out", false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    };
    assert_eq!(got, "46|123");
}

#[test]
fn bytecode_try_catch() {
    // try/catch compiles to the VM: a thrown value / native throw is caught, nested try rethrows to
    // the outer catch, `return` inside try still returns, and — the reason Hono's async `compose`
    // now compiles — a rejected `await` inside a `try` lands in its `catch`.
    let mut e = Engine::new();
    e.interp.tier = crate::bytecode::Tier::Bytecode;
    e.interp.tier_threshold = 0;
    let src = r#"
      function f(x){ try { if (x<0) throw "neg"+x; return "ok"+x; } catch(e){ return "c:"+e; } }
      function native(){ try { null.x; } catch(e){ return e.constructor.name; } }
      function nested(){ try { try { throw "in"; } catch(e){ throw e+"!"; } } catch(e){ return "out:"+e; } }
      function noParam(){ try { throw 1; } catch { return "swallowed"; } }
      var out = "";
      async function ar(x){ try { return await Promise.reject("r"+x); } catch(e){ return "ac:"+e; } }
      async function main(){
        out = [f(2), f(-1), native(), nested(), noParam(), await ar(9)].join("|");
      }
      main();
    "#;
    e.eval(src, false).expect("parse");
    let got = match e.eval("out", false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    };
    assert_eq!(got, "ok2|c:neg-1|TypeError|out:in!|swallowed|ac:r9");
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
        // toFixed rounds half *up* (ties toward the larger n), not half-to-even (issue #5).
        ("(0.5).toFixed(0)", "1"),
        ("(2.5).toFixed(0)", "3"),
        ("(4.5).toFixed(0)", "5"),
        ("(1.25).toFixed(1)", "1.3"),
        ("(-2.5).toFixed(0)", "-3"),
        // Ties are judged on the exact binary64 value: these only *look* like halves, so they
        // round down (0.15 is really 0.1499…, 1.005 is 1.00499…, 8.575 is 8.57499…).
        ("(0.15).toFixed(1)", "0.1"),
        ("(0.35).toFixed(1)", "0.3"),
        ("(0.045).toFixed(2)", "0.04"),
        ("(1.005).toFixed(2)", "1.00"),
        ("(8.575).toFixed(2)", "8.57"),
        ("(9.995).toFixed(2)", "9.99"),
        // Rounding up must propagate the carry across a run of nines.
        ("(0.996).toFixed(2)", "1.00"),
        ("(9.5).toFixed(0)", "10"),
        ("(99.5).toFixed(0)", "100"),
        // Exact expansion at high precision stays faithful (no spurious rounding).
        ("(1234.5678).toFixed(20)", "1234.56780000000003383320"),
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
fn array_iteration_proxy_receiver() {
    // Regression (issue #6): every/some must run [[HasProperty]] through the proxy's traps, not
    // peek at the proxy object's own (empty) property table — otherwise every index reads as a hole
    // and the callback never fires.
    assert_eq!(
        run(
            "var calls=0; var p=new Proxy({length:2,0:'a',1:'b'},{get(o,k){return o[k];}});\
             Array.prototype.every.call(p,function(){calls++;return true;});\
             Array.prototype.some.call(p,function(){calls++;return false;});\
             calls"
        ),
        "4"
    );
    // The `has` trap participates in the hole check: reporting an index absent skips it.
    assert_eq!(
        run(
            "var calls=0; var p=new Proxy({length:3,0:1,1:2,2:3},{has(o,k){return k!=='1';}});\
             Array.prototype.forEach.call(p,function(){calls++;});\
             calls"
        ),
        "2"
    );
    // every short-circuits false and some short-circuits true, both through proxy reads.
    assert_eq!(
        run("var p=new Proxy({length:3,0:2,1:4,2:5},{}); Array.prototype.every.call(p,x=>x%2===0)"),
        "false"
    );
    assert_eq!(
        run("var p=new Proxy({length:3,0:1,1:3,2:4},{}); Array.prototype.some.call(p,x=>x%2===0)"),
        "true"
    );
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
    // No iteration still completes with undefined (ForBodyEvaluation's V starts at undefined).
    assert_eq!(run("2; for(var i=0;i<0;i++){ 3 }"), "undefined");
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
    // An invalid mode is a TypeError (no coercion of the mode value).
    assert_eq!(throws("Iterator.zip([[1]], {mode:'bogus'})"), "TypeError");
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
fn jit_math_sqrt_intrinsic_preserves_fallbacks_and_identity_guards() {
    assert_eq!(
        run_jit(
            "function root(x){return Math.sqrt(x);}
             var original=Math.sqrt, holder={sqrt:original};
             function viaHolder(x){return holder.sqrt(x);}
             for(var i=0;i<600;i++){root(i);viaHolder(i);}
             var coercions=0, object={valueOf:function(){coercions++;return 25;}};
             var before=[root(81),1/root(-0),Number.isNaN(root(-1)),
                         root('16'),root(object),coercions,viaHolder(49)].join(':');
             Math.sqrt=function(x){return x+1;};
             before+':'+root(4)"
        ),
        "9:-Infinity:true:4:5:1:7:5"
    );
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
    // Promise.all element resolve function: name "" length 1 (captured through a custom
    // constructor's synchronous fake-promise `.then`, since a plain thenable's `then` now runs
    // in a microtask per PromiseResolveThenableJob).
    assert_eq!(
        run("var order=[];
             function P(ex){ ex(function(){}, function(){}); }
             P.resolve = function(v){ return { then(f, r) { order.push(f); } }; };
             Promise.all.call(P, [1]);
             var f = order[0]; f.name + ',' + f.length"),
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
fn promise_all_race_use_constructor_capability() {
    fn two(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        let _ = e.eval(setup, false);
        match e.eval(read, false) {
            Ok(Completion::Value(v)) => v,
            Ok(Completion::Throw { name, .. }) => format!("T:{name}"),
            Err(_) => "P".into(),
        }
    }
    // Promise.all routes through a custom constructor's capability resolve, and the resolve-element
    // function's [[AlreadyCalled]] guard makes a second onFulfilled a no-op.
    assert_eq!(
        two(
            "globalThis.count=0;function C(ex){function res(v){globalThis.count++}ex(res,function(){})}C.resolve=function(v){return v};var p1={then:function(f){f('a');f('b')}};Promise.all.call(C,[p1])",
            "count"
        ),
        "1"
    );
    // Native Promise.all still resolves with the values array.
    assert_eq!(
        two(
            "globalThis.r='x';Promise.all([1,Promise.resolve(2),3]).then(a=>{globalThis.r=a.join(',')})",
            "r"
        ),
        "1,2,3"
    );
}

#[test]
fn function_expression_name_is_non_strict_immutable() {
    // Reassigning a named function expression's own name is a silent no-op in sloppy mode.
    assert_eq!(run("var f=function g(){g=1;return g};f()===f"), "true");
    // Under strict mode it throws a TypeError.
    assert_eq!(
        throws("'use strict';var f=function g(){g=1};f()"),
        "TypeError"
    );
    // A const always throws, even in sloppy mode.
    assert_eq!(throws("const x=1;x=2"), "TypeError");
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

#[test]
fn object_literal_proto_setter() {
    // Colon-form __proto__ sets the prototype.
    assert_eq!(
        run("var o={__proto__:Array.prototype};Object.getPrototypeOf(o)===Array.prototype"),
        "true"
    );
    assert_eq!(
        run("var o={__proto__:null};Object.getPrototypeOf(o)"),
        "null"
    );
    // A non-object/null value is ignored (no property, default proto).
    assert_eq!(run("var o={__proto__:5};[o.hasOwnProperty('__proto__'),Object.getPrototypeOf(o)===Object.prototype].join(',')"), "false,true");
    // Quoted key also sets the proto; computed and shorthand do NOT.
    assert_eq!(
        run("var o={'__proto__':Array.prototype};Object.getPrototypeOf(o)===Array.prototype"),
        "true"
    );
    assert_eq!(run("var o={['__proto__']:5};o.__proto__"), "5");
    // Destructuring: __proto__ is a normal keyed read.
    assert_eq!(
        run("var x;({__proto__:x}={['__proto__']:7});String(x)"),
        "7"
    );
}

#[test]
fn iterator_take_closes_on_bad_limit() {
    // A bad take/drop limit closes the underlying iterator (its return() is called).
    assert_eq!(run("var c=0;var o={__proto__:Iterator.prototype,get next(){throw 1},return(){c++;return{}}};try{o.take(NaN)}catch(e){}String(c)"), "1");
    assert_eq!(run("var c=0;var o={__proto__:Iterator.prototype,get next(){throw 1},return(){c++;return{}}};try{o.take(-1)}catch(e){}String(c)"), "1");
    assert_eq!(run("var c=0;var o={__proto__:Iterator.prototype,get next(){throw 1},return(){c++;return{}}};var n='';try{o.take(NaN)}catch(e){n=e.constructor.name}n"), "RangeError");
}

#[test]
fn field_initializer_new_target() {
    assert_eq!(run("class C{x=new.target}String(new C().x)"), "undefined");
    assert_eq!(
        run("class C{x=eval('new.target')}String(new C().x)"),
        "undefined"
    );
}

#[test]
fn static_block_forbids_arguments() {
    assert!(Engine::new()
        .eval("class C{static{arguments}}", false)
        .is_err());
    // super.prop and new.target are still allowed in a static block.
    assert_eq!(run("class B{static m(){return 5}}class C extends B{static y;static{C.y=super.m()}}String(C.y)"), "5");
    assert_eq!(
        run("var r;class C{static{r=String(new.target)}}r"),
        "undefined"
    );
}

#[test]
fn private_member_brand_check() {
    assert_eq!(
        throws("class C{#x=1;static g(o){return o.#x}}C.g({})"),
        "TypeError"
    );
    assert_eq!(
        throws("class C{set #p(v){}static s(o){o.#p=1}}C.s({})"),
        "TypeError"
    );
    assert_eq!(
        throws("class C{#x=1;static c(o){o.#x+=1}}C.c({})"),
        "TypeError"
    );
    // Valid brand access still works.
    assert_eq!(
        run("class C{#x=1;get(){return this.#x}}String(new C().get())"),
        "1"
    );
    assert_eq!(
        run("class C{#x=1;inc(){this.#x++;return this.#x}}String(new C().inc())"),
        "2"
    );
}

#[test]
fn array_mutators_on_primitive_this_are_generic() {
    // Array mutators applied to a primitive `this` operate on the wrapper object
    // (ToObject), not the primitive; in strict mode they'd otherwise throw on [[Set]].
    assert_eq!(run("String(Array.prototype.push.call(true, 1))"), "1");
    assert_eq!(run("String(Array.prototype.pop.call(true))"), "undefined");
    assert_eq!(run("String(Array.prototype.shift.call(true))"), "undefined");
    assert_eq!(run("String(Array.prototype.unshift.call(true, 1))"), "1");
    assert_eq!(
        run("Array.prototype.splice.call(true, 0, 0).length.toString()"),
        "0"
    );
    // And they still mutate real arrays.
    assert_eq!(run("var a=[1,2];a.push(3);a.join(',')"), "1,2,3");
    assert_eq!(run("var a=[1,2,3];a.splice(1,1);a.join(',')"), "1,3");
}

#[test]
fn iterator_prototypes_own_next() {
    // `next` lives on the per-kind iterator prototype (an own property there), not on each
    // iterator instance, and getPrototypeOf² lands on %IteratorPrototype%.
    assert_eq!(
        run("const p = Object.getPrototypeOf([][Symbol.iterator]());
             String(Object.getOwnPropertyDescriptor(p, 'next').value.length)"),
        "0"
    );
    assert_eq!(
        run("const p = Object.getPrototypeOf(''[Symbol.iterator]());
             String(Object.getOwnPropertyDescriptor(p, 'next').value.name)"),
        "next"
    );
    assert_eq!(
        run("const p = Object.getPrototypeOf(''[Symbol.iterator]()); p[Symbol.toStringTag]"),
        "String Iterator"
    );
    // Array and String iterators have distinct prototypes under a shared %IteratorPrototype%.
    assert_eq!(
        run("const ap = Object.getPrototypeOf([][Symbol.iterator]());
             const sp = Object.getPrototypeOf(''[Symbol.iterator]());
             String(ap !== sp && Object.getPrototypeOf(ap) === Object.getPrototypeOf(sp))"),
        "true"
    );
}

#[test]
fn iterator_next_brand_checks() {
    // Calling a prototype `next` with a receiver lacking the matching internal slots throws.
    assert_eq!(
        throws("Object.getPrototypeOf([][Symbol.iterator]()).next.call({})"),
        "TypeError"
    );
    assert_eq!(
        throws("Object.getPrototypeOf(''[Symbol.iterator]()).next.call({})"),
        "TypeError"
    );
    // Cross-kind receivers are also rejected.
    assert_eq!(
        throws("Object.getPrototypeOf([][Symbol.iterator]()).next.call(''[Symbol.iterator]())"),
        "TypeError"
    );
}

#[test]
fn string_iterator_is_lazy_by_code_point() {
    // An astral code point comes out as one iteration step, not two.
    assert_eq!(
        run("const it = 'a\u{1D306}b'[Symbol.iterator](); const o = [];
             for (let r = it.next(); !r.done; r = it.next()) o.push(r.value.codePointAt(0));
             o.join(',')"),
        "97,119558,98"
    );
    // Exhausted iterators stay done.
    assert_eq!(
        run("const it = 'x'[Symbol.iterator](); it.next(); it.next();
             String(it.next().done)"),
        "true"
    );
}

#[test]
fn throw_type_error_single_per_realm() {
    // The same %ThrowTypeError% function object backs strict/unmapped arguments `callee` and the
    // Function.prototype caller/arguments restricted accessors.
    assert_eq!(
        run("const tte = Object.getOwnPropertyDescriptor(function(){'use strict';return arguments}(), 'callee').get;
             const ad = Object.getOwnPropertyDescriptor(Function.prototype, 'arguments');
             const cd = Object.getOwnPropertyDescriptor(Function.prototype, 'caller');
             String(tte === ad.set && tte === cd.set && ad.get === cd.get)"),
        "true"
    );
    // A non-simple parameter list makes the arguments object unmapped: callee is poisoned too.
    assert_eq!(
        run("function f(a = 0){ return arguments; }
             const d = Object.getOwnPropertyDescriptor(f(), 'callee');
             const tte = Object.getOwnPropertyDescriptor(function(){'use strict';return arguments}(), 'callee').get;
             String(d.get === tte && d.set === tte)"),
        "true"
    );
    // Mapped (sloppy, simple params): callee is a data property naming the function itself.
    assert_eq!(
        run("function g(a){ return arguments; }
             String(Object.getOwnPropertyDescriptor(g(), 'callee').value === g)"),
        "true"
    );
}

#[test]
fn async_dispose_settles_via_return_result() {
    fn after(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        e.eval(setup, false).expect("setup");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    // The async-iterator prototype carrying [@@asyncDispose].
    let proto = "Object.getPrototypeOf(Object.getPrototypeOf((async function*(){})()))";
    // A rejected promise from return() rejects the @@asyncDispose promise.
    assert_eq!(
        after(
            &format!(
                "var out = 'pending';
                 const it = Object.create({proto});
                 it.return = () => Promise.reject('boom');
                 it[Symbol.asyncDispose]().then(v => out = 'ok:' + v, e => out = 'rej:' + e);"
            ),
            "out"
        ),
        "rej:boom"
    );
    // A throwing `return` getter rejects (not throws synchronously).
    assert_eq!(
        after(
            &format!(
                "var out = 'pending';
                 const it = Object.create({proto});
                 Object.defineProperty(it, 'return', {{ get() {{ throw 'boom'; }} }});
                 it[Symbol.asyncDispose]().then(v => out = 'ok:' + v, e => out = 'rej:' + e);"
            ),
            "out"
        ),
        "rej:boom"
    );
    // A fulfilled result is dropped: the dispose promise fulfills with undefined.
    assert_eq!(
        after(
            &format!(
                "var out = 'pending';
                 const it = Object.create({proto});
                 it.return = () => Promise.resolve('dropped');
                 it[Symbol.asyncDispose]().then(v => out = 'ok:' + v, e => out = 'rej:' + e);"
            ),
            "out"
        ),
        "ok:undefined"
    );
}

#[test]
fn parse_float_infinity_and_prefix() {
    assert_eq!(run("String(parseFloat('Infinity'))"), "Infinity");
    assert_eq!(run("String(parseFloat('-Infinity'))"), "-Infinity");
    assert_eq!(run("String(parseFloat('+Infinity1'))"), "Infinity");
    // The longest valid literal prefix wins; a dangling exponent marker is not part of it.
    assert_eq!(run("String(parseFloat('1ex'))"), "1");
    assert_eq!(run("String(parseFloat('1e2x'))"), "100");
    assert_eq!(run("String(parseFloat('.5e'))"), "0.5");
    assert_eq!(run("String(parseFloat('e10'))"), "NaN");
    assert_eq!(run("String(parseFloat('-.'))"), "NaN");
}

#[test]
fn parse_int_radix_to_uint32() {
    // The radix goes through ToUint32: Infinity wraps to 0 (-> default 10), 2^32+2 wraps to 2.
    assert_eq!(run("String(parseInt('11', Infinity))"), "11");
    assert_eq!(run("String(parseInt('11', 4294967298))"), "3");
    assert_eq!(run("String(parseInt('11', -4294967294))"), "3");
    assert_eq!(run("String(parseInt('11', 1))"), "NaN");
}

#[test]
fn uri_decode_spec() {
    // decodeURI preserves escapes of the reserved set; decodeURIComponent decodes them.
    assert_eq!(
        run("decodeURI('%3B%2F%3F%3A%40%26%3D%2B%24%2C%23')"),
        "%3B%2F%3F%3A%40%26%3D%2B%24%2C%23"
    );
    assert_eq!(run("decodeURIComponent('%3B%2F')"), ";/");
    assert_eq!(run("decodeURI('%41%62')"), "Ab");
    // Multi-byte sequences decode across escapes; astral code points survive.
    assert_eq!(
        run("decodeURIComponent('%F0%9D%8C%86').codePointAt(0).toString(16)"),
        "1d306"
    );
    assert_eq!(run("decodeURIComponent('%D0%AE')"), "Ю");
    // Malformed input throws URIError: bad hex, truncated, stray continuation, overlong,
    // encoded surrogate, out of range.
    for bad in [
        "'%G1'",
        "'%1'",
        "'%'",
        "'%80'",
        "'%C0%80'",
        "'%ED%A0%80'",
        "'%F5%80%80%80'",
        "'%F0%9D%8C'",
    ] {
        assert_eq!(throws(&format!("decodeURIComponent({bad})")), "URIError");
        assert_eq!(throws(&format!("decodeURI({bad})")), "URIError");
    }
    // A '+' is not a hex digit ("%+1" must not parse as 0x1).
    assert_eq!(throws("decodeURIComponent('%+1')"), "URIError");
}

#[test]
fn from_char_code_combines_surrogate_pairs() {
    assert_eq!(
        run("String.fromCharCode(0xD834, 0xDF06).codePointAt(0).toString(16)"),
        "1d306"
    );
    assert_eq!(run("String.fromCharCode(72, 105)"), "Hi");
    // ToUint16 wrapping still applies.
    assert_eq!(run("String.fromCharCode(65 + 65536)"), "A");
}

#[test]
fn parser_early_errors_operators() {
    // A UnaryExpression (or await expression) cannot be the base of `**`.
    for src in [
        "-1 ** 2",
        "+x ** 2",
        "!x ** 2",
        "~x ** 2",
        "void x ** 2",
        "typeof x ** 2",
        "delete x.y ** 2",
        "async function f(){ await x ** 2 }",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // Parenthesized bases and update-expression bases stay valid.
    assert_eq!(run("(-2) ** 2"), "4");
    assert_eq!(run("var x=2; String(x++ ** 2)"), "4");
    assert_eq!(run("2 ** -1"), "0.5");
}

#[test]
fn parser_early_errors_coalesce_mixing() {
    for src in ["a ?? b || c", "a ?? b && c", "a || b ?? c", "a && b ?? c"] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // Parentheses resolve the ambiguity.
    assert_eq!(run("String((null ?? 'x') || 'y')"), "x");
    assert_eq!(run("String(null ?? ('a' && 'b'))"), "b");
    assert_eq!(run("String((null && 1) ?? 'z')"), "z");
    assert_eq!(run("String(1 ?? 2 ?? 3)"), "1");
}

#[test]
fn parser_early_errors_yield_await_identifiers() {
    for src in [
        "function *g(){ void yield; }",
        "function *g(){ void yi\\u0065ld; }",
        "(function *yield(){})",
        "async function f(){ void aw\\u0061it; }",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // `yield`/`await` stay usable as identifiers outside those contexts (sloppy mode).
    assert_eq!(run("var yield = 3; yield"), "3");
    assert_eq!(run("var await = 4; await"), "4");
    // A generator *declaration*'s name binds in the enclosing (non-generator) scope.
    assert_eq!(
        run("function *yield(){ return 1; } typeof yield"),
        "function"
    );
    // `yield <newline> *` cannot form yield* (ASI splits it).
    assert!(Engine::new()
        .eval("function *g(){ yield\n* 2; }", false)
        .is_err());
}

#[test]
fn proto_dup_literal_vs_pattern() {
    // Two `__proto__:` data properties in an object *literal* are a SyntaxError...
    assert!(Engine::new()
        .eval("({__proto__: 1, __proto__: 2})", false)
        .is_err());
    assert!(Engine::new()
        .eval("var o = { __proto__: null, '__proto__': null };", false)
        .is_err());
    // ...but a destructuring assignment pattern may repeat the key.
    assert_eq!(
        run("var x, y; ({ __proto__: x, __proto__: y } = { a: 1 }); String(x === y)"),
        "true"
    );
    assert_eq!(
        run("var x; ({ __proto__: x } = {}); String(x === Object.prototype)"),
        "true"
    );
}

#[test]
fn statement_completion_values() {
    // eval's completion follows the spec's EMPTY/UpdateEmpty bookkeeping: declarations and
    // value-less statements don't update V, but statements that *complete* with undefined do.
    assert_eq!(run("String(eval('1; var x;'))"), "1");
    assert_eq!(run("String(eval('1; void 0;'))"), "undefined");
    assert_eq!(run("String(eval('var x'))"), "undefined");
    // Loops and ifs complete with undefined when their body produced no value.
    assert_eq!(run("String(eval('1; for (;false;) {}'))"), "undefined");
    assert_eq!(run("String(eval('1; if (true) {}'))"), "undefined");
    assert_eq!(run("String(eval('1; if (false) 2;'))"), "undefined");
    assert_eq!(run("String(eval('1; while (false) {}'))"), "undefined");
    // ...and with the last body value otherwise.
    assert_eq!(
        run("String(eval('for (var r = true; r; r = false) { 3; }'))"),
        "3"
    );
    assert_eq!(run("String(eval('if (true) 2;'))"), "2");
    assert_eq!(run("String(eval('switch (1) { case 1: 4; }'))"), "4");
    assert_eq!(
        run("String(eval('5; switch (1) { case 1: break; }'))"),
        "undefined"
    );
    assert_eq!(run("String(eval('try { 6; } finally {}'))"), "6");
    assert_eq!(run("String(eval('7; try { } catch (e) {}'))"), "undefined");
}

#[test]
fn break_carries_completion_value() {
    // A break threads the statement list's V outward (UpdateEmpty), so the loop/labelled
    // statement completes with the last value produced before the break.
    assert_eq!(run("String(eval('while (true) { 1; break; }'))"), "1");
    assert_eq!(
        run("String(eval('2; while (true) { break; }'))"),
        "undefined"
    );
    assert_eq!(run("String(eval('outer: { 3; break outer; }'))"), "3");
    assert_eq!(run("String(eval('4; outer: { break outer; }'))"), "4");
    assert_eq!(run("String(eval('for (;;) { 5; break; }'))"), "5");
    // An `if` around the break fills the break's empty value with undefined (UpdateEmpty),
    // so the loop completes with undefined, not the earlier 5.
    assert_eq!(
        run("String(eval('for (;;) { 5; if (true) break; }'))"),
        "undefined"
    );
    // continue threads its value into the loop's V as well.
    assert_eq!(
        run("String(eval('var i = 0; while (i < 2) { i++; 6; continue; }'))"),
        "6"
    );
}

#[test]
fn private_names_are_per_class_evaluation() {
    // Two evaluations of the same class source mint distinct private names: an instance of the
    // first fails the brand check inside the second's methods.
    assert_eq!(
        throws(
            "function make() { return class { #m() { return 1; } static call(o) { return o.#m(); } }; }
             const C1 = make(), C2 = make();
             C2.call(new C1())"
        ),
        "TypeError"
    );
    assert_eq!(
        run(
            "function make() { return class { #x = 7; static get(o) { return o.#x; } }; }
             const C1 = make(), C2 = make();
             String(C1.get(new C1()))"
        ),
        "7"
    );
    // #x in o distinguishes evaluations too.
    assert_eq!(
        run(
            "function make() { return class { #x; static has(o) { return #x in o; } }; }
             const C1 = make(), C2 = make();
             String(C1.has(new C1()) && !C2.has(new C1()))"
        ),
        "true"
    );
    // A nested class's private name shadows the outer one: writing through the inner
    // (getter-only) #x on an outer instance is a brand-check TypeError.
    assert_eq!(
        throws(
            "class Outer {
               set #x(v) {}
               static run() {
                 const outer = new Outer();
                 class Inner { get #x() { return 1; } static w(o) { o.#x = 2; } }
                 Inner.w(outer);
               }
             }
             Outer.run()"
        ),
        "TypeError"
    );
    // Private method names still display their source spelling.
    assert_eq!(
        run("class C { #m() {} static n() { return Object.getOwnPropertyNames(C.prototype).length; } } String(C.n())"),
        "1"
    );
}

#[test]
fn fn_name_symbol_keys() {
    // NamedEvaluation with a symbol key: "[description]", or "" without one.
    assert_eq!(
        run("const s = Symbol('test262'); ({ [s]: function(){} })[s].name"),
        "[test262]"
    );
    assert_eq!(
        run("const s = Symbol(); String(({ [s]: function(){} })[s].name)"),
        ""
    );
    assert_eq!(run("const s = Symbol('m'); ({ [s]() {} })[s].name"), "[m]");
    assert_eq!(
        run("const s = Symbol('a');
             Object.getOwnPropertyDescriptor({ get [s]() {} }, s).get.name"),
        "get [a]"
    );
    assert_eq!(run("({ id: function(){} }).id.name"), "id");
}

#[test]
fn private_set_method_and_getter_only() {
    // PrivateSet on a private method is a TypeError (methods are not writable)...
    assert_eq!(
        throws("class C { #m() {} static w(o) { o.#m = 1; } } C.w(new C())"),
        "TypeError"
    );
    assert_eq!(
        throws("class C { #m() {} static w(o) { o.#m += 1; } } C.w(new C())"),
        "TypeError"
    );
    // ...as is writing through a getter-only private accessor (never a sloppy no-op).
    assert_eq!(
        throws("class C { get #x() { return 1; } static w(o) { o.#x = 2; } } C.w(new C())"),
        "TypeError"
    );
    // A private setter still works, and fields stay writable.
    assert_eq!(
        run(
            "class C { #v = 0; set #x(v) { this.#v = v; } get #x() { return this.#v; }
             static rw(o) { o.#x = 5; return o.#x; } } String(C.rw(new C()))"
        ),
        "5"
    );
    assert_eq!(
        run("class C { #f = 1; static rw(o) { o.#f += 2; return o.#f; } } String(C.rw(new C()))"),
        "3"
    );
}

#[test]
fn annexb_function_in_block_hoisting() {
    // B.3.3: a sloppy block function gets a function-scope var binding, initialized to
    // undefined, synced with the block binding when the declaration evaluates.
    assert_eq!(
        run("var r; (function() { eval('r = [typeof f]; { function f() {} } r.push(typeof f);'); }()); r.join(',')"),
        "undefined,function"
    );
    // The block binding is independent: assigning inside the function rebinds the block
    // binding, and the promoted var keeps the function across repeated calls.
    assert_eq!(
        run("var r; (function() { eval('{ function f() { r = [typeof f]; f = 123; r.push(f); return 1; } }f(); f();'); }()); r.join(',')"),
        "number,123"
    );
    // A bare if-position declaration acts as an implicit block (B.3.4).
    assert_eq!(
        run("String((function(){ if (true) function f() { return 1; } return typeof f; })())"),
        "function"
    );
    // An intervening lexical (for-head let, destructured catch param) skips the promotion...
    assert_eq!(
        run("(function() { return eval('for (let f; false; ) {{ function f() {} }} typeof f;'); }())"),
        "undefined"
    );
    assert_eq!(
        run("(function() { return eval('try { throw {}; } catch ({ f }) {{ function f() {} }} typeof f;'); }())"),
        "undefined"
    );
    // ...but a simple catch parameter does not (the B.3.5 legacy exemption).
    assert_eq!(
        run("(function() { return eval('try { throw null; } catch (f) {{ function f() { return 1; } }} typeof f;'); }())"),
        "function"
    );
    // In *function code* (unlike eval code) a same-named parameter blocks the promotion.
    assert_eq!(
        run("(function(f) { { function f() {} } return f; }(123)).toString()"),
        "123"
    );
    // `if (x) function f(){} else function f(){}` after a lexical: legal, promotion skipped.
    assert_eq!(
        run("(function() { return eval('let f = 1; if (true) function f() {} else function _f() {} f;'); }()).toString()"),
        "1"
    );
}

#[test]
fn annexb_html_comments() {
    assert_eq!(
        run("var x = 1; <!-- this is a comment
 x"),
        "1"
    );
    assert_eq!(
        run("var x = 2;
--> a comment
x"),
        "2"
    );
    assert_eq!(
        run("--> comment on the very first line
'ok'"),
        "ok"
    );
    // `a --> b` mid-line is still the two operators.
    assert_eq!(run("var a = 5; var b = 1; String(a-- > b)"), "true");
}

#[test]
fn regexp_class_and_property_escapes() {
    // `[]` is the empty class (never matches); `[^]` matches anything; `[]]` is empty class + ']'.
    assert_eq!(run("String(/[]/.test('a'))"), "false");
    assert_eq!(run("String(/[^]/.test('a'))"), "true");
    assert_eq!(run("String(/[]a/.test('\\0a\\0a'))"), "false");
    assert_eq!(run("String(/x[]]y/.test('x]y'))"), "false");
    // \p{...} uses exact spellings — no UAX44 loose matching.
    assert_eq!(run("String(/\\p{Any}/u.test('a'))"), "true");
    assert_eq!(run("String(/\\p{ASCII}/u.test('a'))"), "true");
    assert_eq!(run("String(/\\p{Assigned}/u.test('a'))"), "true");
    assert_eq!(run("String(/\\P{Assigned}/u.test('\\u{378}'))"), "true");
    for bad in [
        "'\\\\p{any}'",
        "'\\\\p{ASSIGNED}'",
        "'\\\\p{Ascii}'",
        "'\\\\p{gC=uppercase_letter}'",
        "'\\\\p{gc=uppercaseletter}'",
        "'\\\\p{lowercase}'",
    ] {
        assert_eq!(
            throws(&format!("new RegExp({bad}, 'u')")),
            "SyntaxError",
            "should reject {bad}"
        );
    }
    assert_eq!(run("String(/\\p{gc=Lu}/u.test('A'))"), "true");
    assert_eq!(run("String(/\\p{Script=Latin}/u.test('a'))"), "true");
}

#[test]
fn regexp_group_name_surrogate_escapes() {
    // A lead/trail `\u` escape pair in a group name combines into one code point.
    assert_eq!(run("String(/(?<a\\uD801\\uDCA4>.)/u.test('a'))"), "true");
    assert_eq!(run("String(/(?<\\u0041>.)/u.exec('x').groups.A)"), "x");
    assert_eq!(run("String(/(?<a\\u{104A4}>.)/u.test('a'))"), "true");
}

#[test]
fn typed_and_deferred_modules() {
    fn run_mod(files: &[(&str, &str)], entry: &str, read: &str) -> String {
        let mut e = Engine::new();
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let entry_src = files
            .iter()
            .find(|(k, _)| k == entry)
            .map(|(_, v)| v.clone())
            .unwrap();
        e.eval_module(&entry_src, entry, move |spec, _referrer| {
            files
                .iter()
                .find(|(k, _)| k == spec)
                .map(|(k, v)| (k.clone(), v.clone()))
        })
        .expect("parse");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    // JSON modules: default export is the parsed value.
    assert_eq!(
        run_mod(
            &[
                (
                    "main",
                    "import v from 'data' with { type: 'json' }; globalThis.out = v.a;"
                ),
                ("data", "{\"a\": 42}"),
            ],
            "main",
            "String(out)"
        ),
        "42"
    );
    // Text modules: default export is the verbatim source text.
    assert_eq!(
        run_mod(
            &[
                (
                    "main",
                    "import t from 'data' with { type: 'text' }; globalThis.out = t;"
                ),
                ("data", "hello \"world\"\n"),
            ],
            "main",
            "out"
        ),
        "hello \"world\"\n"
    );
    // import defer: evaluation happens on first namespace property access, not at link.
    assert_eq!(
        run_mod(
            &[
                (
                    "main",
                    "import defer * as ns from 'dep'; globalThis.before = globalThis.ran;
                     globalThis.val = ns.x; globalThis.after = globalThis.ran;"
                ),
                ("dep", "globalThis.ran = true; export const x = 7;"),
            ],
            "main",
            "[String(before), String(val), String(after)].join(',')"
        ),
        "undefined,7,true"
    );
}

#[test]
fn mapped_arguments_object() {
    // Sloppy simple-parameter functions get a mapped arguments object: index writes alias
    // the parameters (and vice versa).
    assert_eq!(
        run(
            "function f(a, b) { arguments[0] = 10; b = 'x'; return [a, arguments[1]].join(','); }
             f(1, 2)"
        ),
        "10,x"
    );
    // delete severs the alias.
    assert_eq!(
        run("function f(a) { delete arguments[0]; arguments[0] = 9; return String(a); } f(1)"),
        "1"
    );
    // Strict / non-simple parameter lists are unmapped.
    assert_eq!(
        run("function f(a) { 'use strict'; arguments[0] = 5; return String(a); } f(1)"),
        "1"
    );
    assert_eq!(
        run("function f(a = 0) { arguments[0] = 5; return String(a); } f(1)"),
        "1"
    );
    // Arguments is a real exotic object: [object Arguments], configurable length, iterable.
    assert_eq!(
        run("function f() { return Object.prototype.toString.call(arguments); } f()"),
        "[object Arguments]"
    );
    assert_eq!(
        run(
            "function f() { const d = Object.getOwnPropertyDescriptor(arguments, 'length');
             return [d.value, d.writable, d.enumerable, d.configurable].join(','); } f(1, 2)"
        ),
        "2,true,false,true"
    );
    assert_eq!(
        run("function f() { return [...arguments].join('-'); } f(1, 2, 3)"),
        "1-2-3"
    );
}

#[test]
fn destructuring_and_for_head_early_errors() {
    // A rest element followed by a comma/elision is invalid in a destructuring pattern...
    for src in [
        "var x; [...x,] = [];",
        "var x; [...x, ,] = [];",
        "var x; for ([...x,] in [[]]) ;",
        "'use strict'; [arguments] = [1];",
        "'use strict'; ({ a: eval } = { a: 1 });",
        "'use strict'; for ([arguments] of [[1]]) ;",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // ...but stays a perfectly good spread in an array literal.
    assert_eq!(run("[...[1, 2],].join(',')"), "1,2");
    assert_eq!(run("[...[1], 3].join(',')"), "1,3");
    // A for-in head's right side is a full Expression (comma allowed).
    assert_eq!(
        run("var out = []; for (var k in ({a: 1}, {b: 2})) out.push(k); out.join(',')"),
        "b"
    );
    // Sloppy mode still allows eval/arguments as destructuring targets.
    assert_eq!(run("var eval2; [eval2] = [3]; String(eval2)"), "3");
}

#[test]
fn literal_early_errors() {
    // Escaped keyword spellings are never the keyword.
    for src in ["tru\\u0065", "fals\\u0065", "n\\u0075ll"] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // A numeric literal can't be immediately followed by an identifier start or digit.
    assert!(Engine::new().eval("3in [1]", false).is_err());
    assert!(Engine::new().eval("var x = 1if", false).is_err());
    // Raw U+2028/U+2029 are legal in strings (json-superset); CR/LF are not.
    assert_eq!(run("'\u{2028}' === '\\u2028' ? 'y' : 'n'"), "y");
    assert!(Engine::new().eval("'a\nb'", false).is_err());
    // Line continuations accept every LineTerminatorSequence, including CRLF.
    assert_eq!(run("'a\\\r\nb'"), "ab");
    assert_eq!(run("'a\\\u{2029}b'"), "ab");
}

#[test]
fn directive_prologue_scans_all_directives() {
    // "use strict" anywhere in the prologue makes the whole prologue strict — a legacy
    // octal escape in an *earlier* directive is a SyntaxError.
    for src in [
        "function f() { '\\1'; 'use strict'; }",
        "function f() { '\\8'; 'use strict'; }",
        "'\\1'; 'use strict';",
    ] {
        assert!(
            Engine::new().eval(src, false).is_err(),
            "should reject: {src}"
        );
    }
    // A string after the prologue (or a non-directive continuation) stays sloppy.
    assert_eq!(
        run("function f() { var x; '\\1'; return 1; } String(f())"),
        "1"
    );
    assert_eq!(
        run("var s = '\\1' + 'use strict'; s.length.toString()"),
        "11"
    );
}

#[test]
fn regexp_u_mode_early_errors() {
    for bad in [
        "'{2}'",
        "'.(?<=.)?'",
        "'.(?=.)?', 'u'",
        "'\\\\q', 'u'",
        "'\\\\00', 'u'",
        "'\\\\2', 'u'",
        "'\\\\u{110000}', 'u'",
        "'\\\\u{1F_639}', 'u'",
        "'\\\\uZZ', 'u'",
        "'{', 'u'",
        "'x{2,1}'",
    ] {
        assert_eq!(
            throws(&format!("new RegExp({bad})")),
            "SyntaxError",
            "should reject {bad}"
        );
    }
    // Annex B keeps these legal without the u flag.
    assert_eq!(run("String(/.(?=.)?/.test('ab'))"), "true");
    assert_eq!(run("String(/{/.test('{'))"), "true");
    assert_eq!(run("String(/\\q/.test('q'))"), "true");
}

#[test]
fn regexp_u_surrogates_and_case_mapping() {
    // A surrogate escape pair in /u combines into one code point.
    assert_eq!(run("String(/\\uD834\\uDF06/u.test('\u{1D306}'))"), "true");
    assert_eq!(run("String(/[\\uD834\\uDF06]/u.test('\u{1D306}'))"), "true");
    // Legacy /i never folds a non-ASCII character onto ASCII; /iu does.
    assert_eq!(run("String(/\\u212a/i.test('K'))"), "false");
    assert_eq!(run("String(/\\u212a/iu.test('K'))"), "true");
    assert_eq!(run("String(/k/iu.test('\u{212A}'))"), "true");
    assert_eq!(run("String(/K/i.test('k'))"), "true");
}

#[test]
fn module_bindings_and_source_phase() {
    fn run_mod(files: &[(&str, &str)], entry: &str, read: &str) -> String {
        let mut e = Engine::new();
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let entry_src = files
            .iter()
            .find(|(k, _)| k == entry)
            .map(|(_, v)| v.clone())
            .unwrap();
        e.eval_module(&entry_src, entry, move |spec, _| {
            files
                .iter()
                .find(|(k, _)| k == spec)
                .map(|(k, v)| (k.clone(), v.clone()))
        })
        .expect("parse");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    // Import bindings are immutable: reads are live, assignment is a TypeError.
    assert_eq!(
        run_mod(
            &[(
                "m",
                "import { f as f2 } from 'm'; export function f() { return 23; }
                 try { f2 = null; globalThis.out = 'no-throw'; }
                 catch (e) { globalThis.out = 'threw:' + (e instanceof TypeError); }"
            )],
            "m",
            "out"
        ),
        "threw:true"
    );
    // `import source x` binds a ModuleSource object; `import source from 'm'` is a default
    // import named `source`; both parse alongside `import from from`-style bindings.
    assert_eq!(
        run_mod(
            &[(
                "m",
                "import source x from '<module source>';
                 globalThis.out = typeof x + ':' + (x === Object($262.AbstractModuleSource ? x : x));"
            )],
            "m",
            "out"
        ),
        "object:true"
    );
    assert_eq!(
        run_mod(
            &[
                ("m", "import source from 'dep'; globalThis.out = source;"),
                ("dep", "export default 'dflt';"),
            ],
            "m",
            "out"
        ),
        "dflt"
    );
    // Two star-exported source bindings of the same specifier are unambiguous.
    assert_eq!(
        run_mod(
            &[
                (
                    "m",
                    "import * as ns from 'both'; globalThis.out = typeof ns.mod;"
                ),
                ("both", "export * from 'a'; export * from 'b';"),
                (
                    "a",
                    "import source mod from '<module source>'; export { mod };"
                ),
                (
                    "b",
                    "import source mod from '<module source>'; export { mod };"
                ),
            ],
            "m",
            "out"
        ),
        "object"
    );
}

#[test]
fn super_set_and_constructor_return_override() {
    // A base constructor returning an object overrides `this`; super.x = v walks the super
    // base's chain (a setter there wins) and otherwise defines on the receiver.
    assert_eq!(
        run("var got;
             class A { constructor() { return { marker: 1 }; } set foo(v) { got = v; } }
             class B extends A { constructor() { super(); super.foo = 14; } }
             new B(); String(got)"),
        "14"
    );
    assert_eq!(
        run("class A { constructor() { return { }; } }
             class B extends A { constructor() { super(); this.x = 5; } }
             String(new B().x)"),
        "5"
    );
    assert_eq!(
        run("class C { constructor() { return { y: 9 }; } } String(new C().y)"),
        "9"
    );
}

#[test]
fn dynamic_import_top_level_await() {
    let mut e = Engine::new();
    let files: Vec<(String, String)> = vec![(
        "tla".to_string(),
        "globalThis.started = true; await globalThis.gate; globalThis.finished = true;".to_string(),
    )];
    e.set_module_loader(move |spec: &str, _referrer: &str| {
        files
            .iter()
            .find(|(k, _)| k == spec)
            .map(|(k, v)| (k.clone(), v.clone()))
    });
    e.eval(
        "var resolveGate; globalThis.gate = new Promise(r => resolveGate = r);
         globalThis.order = [];
         import('tla').then(() => order.push('ns'));
         globalThis.kick = () => resolveGate();",
        false,
    )
    .expect("setup");
    // The module starts synchronously but suspends at the top-level await.
    match e
        .eval("String(started) + ':' + String(globalThis.finished)", false)
        .expect("read")
    {
        Completion::Value(v) => assert_eq!(v, "true:undefined"),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
    // Releasing the gate finishes evaluation and settles the import promise.
    match e.eval("kick(); undefined", false).expect("kick") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
    match e
        .eval("String(finished) + ':' + order.join(',')", false)
        .expect("read2")
    {
        Completion::Value(v) => assert_eq!(v, "true:ns"),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

#[test]
fn small_area_conformance_fixes() {
    // U+FEFF is whitespace anywhere in the source.
    assert_eq!(run("var re = /x/g\u{FEFF}; typeof re"), "object");
    // A computed static class member key evaluating to "prototype" is a TypeError.
    assert_eq!(
        throws("var k = 'prototype'; class C { static [k]() {} }"),
        "TypeError"
    );
    assert_eq!(
        run("class C { static ['ok']() { return 1; } } String(C.ok())"),
        "1"
    );
    // WeakRef exposes no own properties for its target.
    assert_eq!(
        run("String(Object.getOwnPropertyNames(new WeakRef({})).length)"),
        "0"
    );
    assert_eq!(
        run("var o = {}; String(new WeakRef(o).deref() === o)"),
        "true"
    );
    // An escaped "use strict" is not a directive; a clean one after other directives is.
    assert_eq!(
        run("function f() { 'use\\u0020strict'; return this !== undefined; } String(f())"),
        "true"
    );
    // `undefined = v` parses; strict mode throws at runtime.
    assert_eq!(throws("'use strict'; undefined = 12;"), "TypeError");
    assert_eq!(run("undefined = 12; 'ok'"), "ok");
    // `await` is fully reserved in class static blocks (but fine in nested functions).
    assert!(Engine::new()
        .eval("class C { static { await; } }", false)
        .is_err());
    assert!(Engine::new()
        .eval("class C { static { await 1; } }", false)
        .is_err());
    assert_eq!(
        run("class C { static { function g(await) { return await; } C.v = g(5); } } String(C.v)"),
        "5"
    );
    // A body-top function declaration may share a parameter's name.
    assert_eq!(
        run("function f(x) { return typeof x; function x() {} } f(1)"),
        "function"
    );
    // A regex may open right after a class declaration's body.
    assert_eq!(run("class A {}/1/.source"), "1");
    // ...while division after an object literal (value position) still wins.
    assert_eq!(run("var n = 6, r = { v: 4 } / n / 2; String(r)"), "NaN");
    // A setter on a wrapper prototype runs for a primitive base, receiver included.
    assert_eq!(
        run("var got; Object.defineProperty(Number.prototype, 'p', { set(v) { got = typeof this + ':' + v; } });
             (5).p = 7; got"),
        "object:7" // sloppy-mode receiver boxing; the setter itself ran with the primitive base
    );
}

#[test]
fn sub_ten_area_fixes() {
    // BigInt: constructor coercion + toString radix/length.
    assert_eq!(throws("BigInt(Infinity)"), "RangeError");
    assert_eq!(throws("BigInt(1.5)"), "RangeError");
    assert_eq!(run("String(BigInt({ valueOf: () => 42 }))"), "42");
    assert_eq!(throws("(1n).toString(1)"), "RangeError");
    assert_eq!(run("String(BigInt.prototype.toString.length)"), "0");
    // FinalizationRegistry tracks registrations; internal slots stay hidden.
    assert_eq!(
        run(
            "const fr = new FinalizationRegistry(() => {}); const t = {};
             fr.register({}, 1, t);
             [fr.unregister(t), fr.unregister(t), Object.getOwnPropertyNames(fr).length].join(',')"
        ),
        "true,false,0"
    );
    // JSON: rawJSON exposes only its own property; wrappers re-coerce via valueOf/toString.
    assert_eq!(
        run("Object.getOwnPropertyNames(JSON.rawJSON('1')).join(',')"),
        "rawJSON"
    );
    assert_eq!(
        run("var n = new Number(1); n.valueOf = () => 2; JSON.stringify([n])"),
        "[2]"
    );
    // delete undefined is false (non-configurable global).
    assert_eq!(run("String(delete undefined)"), "false");
    // SharedArrayBuffer: option validation before allocation, negative maxByteLength rejected.
    assert_eq!(
        throws("new SharedArrayBuffer(0, { maxByteLength: -1 })"),
        "RangeError"
    );
    assert_eq!(
        run("String(new SharedArrayBuffer(4, { maxByteLength: 8 }).growable)"),
        "true"
    );
    // Async generators queue overlapping requests (two nexts issued synchronously).
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
            "var out = [];
             async function* g() { yield 1; }
             const it = g();
             it.next().then(r => out.push(r.value, r.done));
             it.next().then(r => out.push(r.value, r.done));",
            "out.join(',')"
        ),
        "1,false,,true"
    );
    // Array.prototype.toLocaleString forwards locales/options to elements.
    assert_eq!(
        run(
            "var got; var el = { toLocaleString(l, o) { got = l + ':' + o.style; return 'x'; } };
             [el].toLocaleString('th', { style: 'decimal' }); got"
        ),
        "th:decimal"
    );
}

#[test]
fn cross_realm_calls_and_constructs() {
    // A function from another realm runs with its own realm's intrinsics: its thrown
    // TypeError is that realm's, distinct from ours.
    assert_eq!(
        run("const other = $262.createRealm().global;
             const otherTte = Object.getOwnPropertyDescriptor(
                 new other.Function('\"use strict\"; return arguments;')(), 'callee').get;
             let cross = false, distinct = false;
             try { otherTte(); } catch (e) {
               cross = e instanceof other.TypeError && !(e instanceof TypeError);
             }
             distinct = otherTte !== Object.getOwnPropertyDescriptor(
                 (function() { 'use strict'; return arguments; })(), 'callee').get;
             String(cross && distinct)"),
        "true"
    );
    // GetPrototypeFromConstructor falls back to the *newTarget's realm's* intrinsic.
    assert_eq!(
        run("const other = $262.createRealm().global;
             const C = new other.Function(); C.prototype = null;
             const o = Reflect.construct(Boolean, [], C);
             String(Object.getPrototypeOf(o) === other.Boolean.prototype)"),
        "true"
    );
    // Cross-realm eval sees its own globals while closures keep resolving in theirs.
    assert_eq!(
        run("const other = $262.createRealm().global;
             other.eval('globalThis.marker = 7;');
             String(other.marker) + ':' + String(typeof globalThis.marker)"),
        "7:undefined"
    );
}

#[test]
fn regexp_v_flag_class_sets() {
    // Set operations: difference, intersection, nested classes.
    assert_eq!(run("String(/[\\d--[0-5]]/v.test('7'))"), "true");
    assert_eq!(run("String(/[\\d--[0-5]]/v.test('3'))"), "false");
    assert_eq!(run("String(/[\\w&&\\d]/v.test('5'))"), "true");
    assert_eq!(run("String(/[\\w&&\\d]/v.test('a'))"), "false");
    assert_eq!(run("String(/[[a-z]--[aeiou]]/v.test('b'))"), "true");
    assert_eq!(run("String(/[[a-z]--[aeiou]]/v.test('e'))"), "false");
    // String disjunctions match longest-first.
    assert_eq!(run("/[\\q{a|bc|abc}]/v.exec('abcd')[0]"), "abc");
    assert_eq!(run("String(/[\\q{ab|cd}x]/v.test('x'))"), "true");
    // Negation of a plain set works; negating a set with strings is a SyntaxError.
    assert_eq!(run("String(/[^\\q{a}b]/v.test('c'))"), "true");
    assert_eq!(throws("new RegExp('[^\\\\q{ab}]', 'v')"), "SyntaxError");
    // Properties of strings (derived sets) match whole sequences.
    assert_eq!(
        run("String(/^\\p{Emoji_Keycap_Sequence}$/v.test('1\\uFE0F\\u20E3'))"),
        "true"
    );
    assert_eq!(
        run("String(/^\\p{Basic_Emoji}$/v.test('\\u{1F600}'))"),
        "true"
    );
    // Reserved syntax in v-classes.
    assert_eq!(throws("new RegExp('[&&]', 'v')"), "SyntaxError");
    assert_eq!(throws("new RegExp('[a--]', 'v')"), "SyntaxError");
    assert_eq!(run("String(/[&]/v.test('&'))"), "true");
}

#[test]
fn temporal_duration_arithmetic_and_parsing() {
    // Fractional ISO components spread exactly into sub-units.
    assert_eq!(run("Temporal.Duration.from('PT0.5H').toString()"), "PT30M");
    assert_eq!(
        run("String(Temporal.Duration.from('PT0.5H').minutes)"),
        "30"
    );
    assert_eq!(
        run("String(Temporal.Duration.from('PT1.5S').milliseconds)"),
        "500"
    );
    // A fraction is only allowed on the last component; order is enforced.
    for bad in ["'PT0.5H30M'", "'P1D2Y'", "'P'", "'PT'", "'P1DT'"] {
        assert_eq!(
            throws(&format!("Temporal.Duration.from({bad})")),
            "RangeError",
            "should reject {bad}"
        );
    }
    // add/subtract balance through total nanoseconds and reject calendar units.
    assert_eq!(
        run("Temporal.Duration.from({ hours: 1 }).add({ minutes: -30 }).toString()"),
        "PT30M"
    );
    assert_eq!(
        run("Temporal.Duration.from({ days: 1 }).subtract({ hours: 36 }).toString()"),
        "-PT12H"
    );
    assert_eq!(
        throws("Temporal.Duration.from({ years: 1 }).add({ hours: 1 })"),
        "RangeError"
    );
}
#[test]
fn resizable_typed_array_integrity() {
    assert_eq!(
        run("const gsab = new SharedArrayBuffer(8, {maxByteLength: 16});
             let r = [];
             try { Object.preventExtensions(new Uint8Array(gsab)); r.push('no-throw'); } catch(e) { r.push(e.name); }
             try { Object.preventExtensions(new Uint8Array(gsab, 0, 4)); r.push('ok'); } catch(e) { r.push(e.name); }
             class MyU8 extends Uint8Array {}
             const rab = new ArrayBuffer(8, {maxByteLength: 16});
             try { Object.preventExtensions(new MyU8(rab, 0, 4)); r.push('no-throw'); } catch(e) { r.push(e.name); }
             try { Object.seal(new Uint8Array(gsab, 0, 4)); r.push('no-throw'); } catch(e) { r.push(e.name); }
             Object.seal(new Uint8Array(gsab, 0, 0)); r.push('sealed-empty');
             r.join(',')"),
        "TypeError,ok,TypeError,TypeError,sealed-empty"
    );
    assert_eq!(
        run("const rab = new ArrayBuffer(8, {maxByteLength: 16});
             const ta = new Uint8Array(rab);
             let r = [];
             try { Object.preventExtensions(ta); r.push('no-throw'); } catch(e) { r.push(e.name); }
             r.push(Reflect.preventExtensions(ta));
             r.push(Reflect.preventExtensions({}) );
             r.join(',')"),
        "TypeError,false,true"
    );
    // The value coercion in a TypedArray write runs before the bounds check, so a coercion that
    // grows the buffer makes the write land.
    assert_eq!(
        run("const rab = new ArrayBuffer(0, {maxByteLength: 4});
             const ta = new Int8Array(rab);
             ta[1] = { valueOf() { rab.resize(4); return 7; } };
             ta[1]"),
        "7"
    );
}

#[test]
fn regexp_duplicate_named_groups_matching() {
    assert_eq!(
        run(r#"JSON.stringify(/(?:(?:(?<a>x)|(?<a>y))\k<a>){2}/.exec('xxyy'))"#),
        r#"["xxyy",null,"y"]"#
    );
    assert_eq!(
        run(r#"'abXcdX'.replace(/(?<d>ab)|(?<d>cd)/g, '[$<d>]')"#),
        "[ab]X[cd]X"
    );
    // Quantifier iterations reset the captures inside the repeated atom.
    assert_eq!(
        run(r#"JSON.stringify(/(?:(a)|(b)){2}/.exec('ab'))"#),
        r#"["ab",null,"b"]"#
    );
}

#[test]
fn uint8array_base64_hex_spec() {
    assert_eq!(
        throws("Uint8Array.fromBase64('SGVsbG8=', {lastChunkHandling: 'stric'})"),
        "TypeError"
    );
    assert_eq!(
        throws("Uint8Array.fromBase64('SGVsbA', {lastChunkHandling: 'strict'})"),
        "SyntaxError"
    );
    assert_eq!(
        run("Uint8Array.fromBase64('SGVsbA', {lastChunkHandling: 'stop-before-partial'}).length"),
        "3"
    );
    assert_eq!(
        run("Uint8Array.fromBase64('SGVsbA').join(',')"),
        "72,101,108,108"
    ); // loose
    assert_eq!(
        throws("Uint8Array.fromBase64('SGVsbG8=extra')"),
        "SyntaxError"
    );
    assert_eq!(
        run("const ta = new Uint8Array(3);
             const r = ta.setFromBase64('SGVsbG8gV29ybGQ=', {lastChunkHandling: 'loose'});
             r.read + ':' + r.written + ':' + ta.join(',')"),
        "4:3:72,101,108"
    );
    assert_eq!(
        run("const ta = new Uint8Array(2);
             const r = ta.setFromHex('aabbcc');
             r.read + ':' + r.written + ':' + ta.join(',')"),
        "4:2:170,187"
    );
    assert_eq!(
        throws("new Uint8Array(2).setFromHex('aabbc')"),
        "SyntaxError"
    );
}

#[cfg(feature = "intl")]
#[test]
fn listformat_to_parts_and_temporal_removed_methods() {
    assert_eq!(
        run(
            "const lf = new Intl.ListFormat('en-US', {type: 'disjunction'});
             lf.formatToParts(['f','o','o']).map(p => p.type[0] + p.value).join('|')"
        ),
        "ef|l, |eo|l, or |eo"
    );
    assert_eq!(
        run("['withPlainDate' in Temporal.PlainDateTime.prototype,
             'epochSeconds' in Temporal.ZonedDateTime.prototype,
             'toPlainMonthDay' in Temporal.ZonedDateTime.prototype].join(',')"),
        "false,false,false"
    );
}

#[test]
fn async_generator_return_awaits_value() {
    fn after(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        e.eval(setup, false).expect("setup");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    // return() while suspendedStart awaits its argument; the result value is the unwrapped one.
    assert_eq!(
        after(
            "var out = '';
             async function* g() { yield 1; }
             const it = g();
             it.return(Promise.resolve('unwrapped')).then(r => { out = r.value + ':' + r.done; });",
            "out"
        ),
        "unwrapped:true"
    );
    // next/return/throw on a non-async-generator receiver reject rather than throw.
    assert_eq!(
        after(
            "var name = '';
             async function* g() {}
             g.prototype.next.call({}).catch(e => { name = e.constructor.name; });",
            "name"
        ),
        "TypeError"
    );
}

#[test]
fn async_from_sync_close_on_rejection() {
    fn after(setup: &str, read: &str) -> String {
        let mut e = Engine::new();
        e.eval(setup, false).expect("setup");
        match e.eval(read, false).expect("read") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
    // A rejected value-promise from a sync iterator closes it (return() runs once).
    assert_eq!(
        after(
            "var returns = 0, caught = '';
             const sync = {
               [Symbol.iterator]() {
                 return {
                   next() { return { value: Promise.reject('nope'), done: false }; },
                   return() { returns += 1; return { done: true }; }
                 };
               }
             };
             (async () => { for await (const _ of sync); })().catch(e => { caught = e; });",
            "returns + ':' + caught"
        ),
        "1:nope"
    );
    // Breaking a for-await over a sync source calls return() with no arguments.
    assert_eq!(
        after(
            "var len = -1;
             const sync = {
               [Symbol.iterator]() { return this; },
               next() { return { done: false }; },
               return() { len = arguments.length; return { done: true }; }
             };
             (async () => { for await (const _ of sync) break; })();",
            "len"
        ),
        "0"
    );
}
#[test]
fn global_declaration_instantiation() {
    assert_eq!(
        run("let gLet = 1;
             let r = '';
             try { $262.evalScript('var gLet;'); r = 'no-throw'; } catch (e) { r = e.constructor.name; }
             r"),
        "SyntaxError"
    );
    assert_eq!(
        run("var test262Var;
             let test262Let;
             $262.evalScript('var test262Var;');
             $262.evalScript('function test262Var() {}');
             let r = '';
             try { $262.evalScript('var x; var test262Let;'); r = 'no-throw'; } catch (e) { r = e.constructor.name; }
             let r2 = '';
             try { x; r2 = 'x-exists'; } catch (e) { r2 = e.constructor.name; }
             r + ':' + r2"),
        "SyntaxError:ReferenceError"
    );
    // Restricted globals and global-object own properties for script declarations.
    assert_eq!(throws("$262.evalScript('let undefined;')"), "SyntaxError");
    assert_eq!(
        run("$262.evalScript('function gFn() {}');
             const d = Object.getOwnPropertyDescriptor(globalThis, 'gFn');
             [typeof d.value, d.writable, d.enumerable, d.configurable].join(',')"),
        "function,true,true,false"
    );
}

#[test]
fn block_scope_redeclaration_early_errors() {
    fn parse_err(src: &str) -> bool {
        Engine::new().eval(src, false).is_err()
    }
    assert!(parse_err("{ var f; function f() {} }"));
    assert!(parse_err("{ function f() {} var f; }"));
    assert!(parse_err("{ function f() {} { var f; } }"));
    assert!(parse_err("{ { var f; } function f() {} }"));
    assert!(parse_err("{ { var f; } let f; }"));
    assert!(!parse_err("{ function f() {} function f() {} }")); // sloppy duplicates OK
    assert!(!parse_err("var f; function f() {} ")); // top level OK
    assert!(!parse_err("let f; { function f() {} }")); // Annex B shadowing OK
                                                       // super()/new.target restrictions in global code.
    assert!(parse_err("super();"));
    assert!(parse_err("() => { super(); };"));
    assert!(parse_err("() => { new.target; };"));
    assert!(!parse_err("function g() { () => new.target; }"));
}

#[test]
fn disposable_stack_semantics() {
    // Distinct brands: a DisposableStack method rejects an AsyncDisposableStack receiver.
    assert_eq!(
        run("let r = '';
             try { DisposableStack.prototype.dispose.call(new AsyncDisposableStack()); r = 'no'; }
             catch (e) { r = e.constructor.name; }
             r"),
        "TypeError"
    );
    // Multiple disposal errors fold into a SuppressedError chain (later error on top).
    assert_eq!(
        run("const s = new DisposableStack();
             s.defer(() => { throw 'first'; });
             s.defer(() => { throw 'second'; });
             let r = '';
             try { s.dispose(); } catch (e) {
               r = e.constructor.name + ':' + e.error + ':' + e.suppressed;
             }
             r"),
        "SuppressedError:first:second"
    );
    // using in a sync function body and a class static block dispose at exit.
    assert_eq!(
        run("let out = [];
             function f() { using x = { [Symbol.dispose]() { out.push('d'); } }; out.push('b'); }
             f();
             class C { static { using y = { [Symbol.dispose]() { out.push('s'); } }; } }
             out.join(',')"),
        "b,d,s"
    );
}
#[test]
fn proxy_forwarding_and_newtarget() {
    // for-of over a proxy of an array
    assert_eq!(
        run("const p = new Proxy([1,2,3], {});
             let out = [];
             for (const x of p) out.push(x);
             out.join(',')"),
        "1,2,3"
    );
    // construct through nested trap-less proxies preserves new.target
    assert_eq!(
        run("const AT = new Proxy(Array, {});
             const AP = new Proxy(AT, {});
             const a = new AP(1,2,3);
             Array.isArray(a) + ':' + a.join(',')"),
        "true:1,2,3"
    );
    assert_eq!(
        run(
            "class MyArray extends Array { get isMyArray() { return true; } }
             const AP = new Proxy(new Proxy(Array, {}), {});
             const m = Reflect.construct(AP, [], MyArray);
             Array.isArray(m) + ':' + (m instanceof MyArray) + ':' + m.isMyArray"
        ),
        "true:true:true"
    );
}
#[test]
fn array_literal_elements_are_own_props() {
    assert_eq!(
        run(
            "Object.defineProperty(Array.prototype, '0', { get(){return 9}, configurable:true });
             const r = [11][0] + ':' + [11].every(v => v === 11) + ':' + [11].indexOf(11);
             delete Array.prototype[0];
             r"
        ),
        "11:true:0"
    );
}
#[test]
fn array_length_set_coercion_order() {
    assert_eq!(
        run("var array = [1, 2, 3];
             var hints = [];
             var length = {};
             length[Symbol.toPrimitive] = function(hint) {
               hints.push(hint);
               Object.defineProperty(array, 'length', {writable: false});
               return 0;
             };
             var r = '' + Reflect.set(array, 'length', length);
             r + ':' + hints.join(',') + ':' + array.length"),
        "false:number,number:3"
    );
}

#[test]
fn array_spec_semantics_batch() {
    // concat: spreadable holes advance the index; result length is set; boxed receiver.
    assert_eq!(
        run("const sp = { length: 3, 0: 'a', 2: 'c' };
             sp[Symbol.isConcatSpreadable] = true;
             const r = [].concat(sp);
             r.length + ':' + (1 in r) + ':' + r.join(',')"),
        "3:false:a,,c"
    );
    assert_eq!(
        run("(Array.prototype.concat.call(true)[0] instanceof Boolean) + ''"),
        "true"
    );
    // duplicate parameter names: only the last occurrence is mapped.
    assert_eq!(
        run(
            "const a = (function (x, x, x) { return arguments; })(1, 2, 3);
             a[Symbol.isConcatSpreadable] = true;
             [].concat(a).join(',') + ':' + a[0] + a[1] + a[2]"
        ),
        "1,2,3:123"
    );
    // toSpliced with no arguments copies everything.
    assert_eq!(run("['a','b','c'].toSpliced().join(',')"), "a,b,c");
    // with() truncates a fractional index and never reads the replaced element.
    assert_eq!(run("[1, 2, 3].with(-0.5, 9).join(',')"), "9,2,3");
    // ArraySetLength: negative or fractional lengths RangeError even via defineProperty.
    assert_eq!(
        run("let r = '';
             try { Object.defineProperty([], 'length', { value: -1, configurable: true }); }
             catch (e) { r = e.constructor.name; }
             r"),
        "RangeError"
    );
    // Array.from constructs the custom receiver before iterating.
    assert_eq!(
        run("let log = [];
             function C() { log.push('ctor'); }
             const obj = { [Symbol.iterator]() { log.push('iter'); return [][Symbol.iterator](); } };
             Array.from.call(C, obj);
             log.join(',')"),
        "ctor,iter"
    );
    // Array.of falls back to a plain array for a non-constructor receiver.
    assert_eq!(
        run("(Array.of.call(Math.cos.bind(Math)) instanceof Array) + ''"),
        "true"
    );
}
#[test]
fn mapped_arguments_define_semantics() {
    assert_eq!(
        run(
            "(function(a){ Object.defineProperty(arguments,'0',{configurable:false});
             let r = [];
             try { delete arguments[0]; r.push('del-ok'); } catch(e){ r.push(e.constructor.name); }
             r.push(Object.prototype.hasOwnProperty.call(arguments,'0'));
             r.push(Object.getOwnPropertyDescriptor(arguments,'0').configurable);
             for (var x in arguments) r.push('in:'+x);
             arguments[0] = 99; r.push(a);
             return r.join(',');
             })(1)"
        ),
        "del-ok,true,false,in:0,99"
    );
    // isWritable-style mutation before the configurable probe (harness order).
    assert_eq!(
        run(
            "(function(a){ Object.defineProperty(arguments,'0',{configurable:false});
             var d0 = Object.getOwnPropertyDescriptor(arguments,'0');
             var unlikely = '__val';
             arguments[0] = unlikely;            // isWritable write
             var w = arguments[0] === unlikely;
             arguments[0] = 1;                   // isWritable restore
             try { delete arguments[0]; } catch(e){}
             var own = Object.prototype.hasOwnProperty.call(arguments,'0');
             return d0.configurable + ',' + w + ',' + own;
             })(1)"
        ),
        "false,true,true"
    );
    assert_eq!(
        run("(function(a) {
             Object.defineProperty(arguments, '0', { configurable: false });
             const d = Object.getOwnPropertyDescriptor(arguments, '0');
             a = 2;
             const d2 = Object.getOwnPropertyDescriptor(arguments, '0');
             return d.configurable + ':' + d2.value + ':' + arguments[0];
             })(1)"),
        "false:2:2"
    );
}
#[test]
fn dbg_slice_to_immutable() {
    assert_eq!(
        run("const ab = new ArrayBuffer(8);
             const calls = [];
             const st = { valueOf() { calls.push('s'); return -1; } };
             const en = { valueOf() { calls.push('e'); return '33'; } };
             const d = ab.sliceToImmutable(st, en);
             calls.join(',') + ':' + d.byteLength"),
        "s,e:1"
    );
    assert_eq!(
        run("const ab2 = new ArrayBuffer(32);
             const d2 = ab2.sliceToImmutable({ [Symbol.toPrimitive]: () => -1 }, { [Symbol.toPrimitive]: () => '-Infinity' });
             '' + d2.byteLength"),
        "0"
    );
    // Assigned (not literal) @@toPrimitive, with poisoned valueOf/toString fallbacks present.
    assert_eq!(
        run("const calls = [];
             const objStart = { valueOf() { calls.push('sv'); return {}; }, toString() { calls.push('st'); return {}; } };
             const objEnd = { valueOf() { calls.push('ev'); return {}; }, toString() { calls.push('et'); return {}; } };
             objStart[Symbol.toPrimitive] = function (h) { calls.push('sp:' + h); return -1; };
             objEnd[Symbol.toPrimitive] = function (h) { calls.push('ep:' + h); return '-Infinity'; };
             const src = new ArrayBuffer(32);
             const d = src.sliceToImmutable(objStart, objEnd);
             calls.join(',') + ':' + d.byteLength"),
        "sp:number,ep:number:0"
    );
    // Full harness-like sequence with closures capturing a reassigned `calls` variable.
    assert_eq!(
        run("var calls = [];
             var rawStart = true, rawEnd = 1;
             var badStartValueOf = false, badStartToString = false;
             var objStart = {
               valueOf() { calls.push('start.valueOf'); return badStartValueOf ? {} : rawStart; },
               toString() { calls.push('start.toString'); return badStartToString ? {} : rawStart; }
             };
             var objEnd = {
               valueOf() { calls.push('end.valueOf'); return rawEnd; },
               toString() { calls.push('end.toString'); return rawEnd; }
             };
             var src = new ArrayBuffer(32);
             src.sliceToImmutable(objStart, objEnd);
             var first = calls.join('|');
             calls = [];
             objEnd[Symbol.toPrimitive] = function(h) { calls.push('end[tp](' + h + ')'); return rawEnd; };
             src.sliceToImmutable(objStart, objEnd);
             var second = calls.join('|');
             badStartToString = true;
             calls = [];
             objStart[Symbol.toPrimitive] = function(h) { calls.push('start[tp](' + h + ')'); return rawStart; };
             src.sliceToImmutable(objStart, objEnd);
             first + ' / ' + second + ' / ' + calls.join('|')"),
        "start.valueOf|end.valueOf / start.valueOf|end[tp](number) / start[tp](number)|end[tp](number)"
    );
}

#[test]
fn gc_side_table_pinning() {
    // Churn enough objects with side-table entries (buffers, views, symbol-keyed coercion
    // closures) to cross the GC trigger; recycled addresses must not inherit stale metadata.
    assert_eq!(
        run("var bad = 0;
             for (var i = 0; i < 40000; i++) {
               var calls = [];
               var src = new ArrayBuffer(8);
               var view = new Uint8Array(src);
               view[0] = 1; view[1] = 2; view[2] = 3;
               var s = { valueOf: function () { calls.push('s'); return 1; } };
               var e = {};
               e[Symbol.toPrimitive] = function (h) { calls.push('e'); return 3; };
               var dest = src.sliceToImmutable(s, e);
               var got = Array.from(new Uint8Array(dest)).join(',');
               if (dest.byteLength !== 2 || got !== '2,3' || calls.join('') !== 'se') { bad++; if (bad > 3) break; }
             }
             '' + bad"),
        "0"
    );
}
#[test]
fn utf16_semantics() {
    assert_eq!(
        run("const s = String.fromCharCode(0xD800, 0xDC00);
             s.length + ':' + encodeURI(s) + ':' + (s === '\\u{10000}')"),
        "2:%F0%90%80%80:true"
    );
    assert_eq!(
        run("let bad = '';
             const chars = [0xDC00, 0xDDFF, 0xDFFF];
             for (let hi = 0xD800; hi <= 0xDBFF; hi++) {
               for (const lo of chars) {
                 const s = String.fromCharCode(hi, lo);
                 try { encodeURI(s); } catch (e) { bad += hi.toString(16) + '/' + lo.toString(16) + ' '; }
               }
             }
             bad.slice(0, 40)"),
        ""
    );
    // Lone surrogates survive round trips, and pairs canonicalize across concatenation.
    assert_eq!(
        run("const lone = String.fromCharCode(0xD83D);
             lone.length + ':' + lone.charCodeAt(0).toString(16) + ':' + (lone === '\\uD83D')
             + ':' + JSON.stringify(lone) + ':' + ('\\uD834' + '\\uDF06' === '\\uD834\\uDF06')
             + ':' + '\u{1D306}'.length + ':' + [...'\u{1D306}'].length"),
        "1:d83d:true:\"\\ud83d\":true:2:1"
    );
    assert_eq!(run("'x'.codePointAt(-1) + ''"), "undefined");
    assert_eq!(run("('\\uD834\\uDF06').split('').length + ''"), "2");
    assert_eq!(
        run("String.prototype.isWellFormed.call(String.fromCharCode(0xD800)) + ''"),
        "false"
    );
}
#[test]
fn shadow_realm_cross_calls() {
    assert_eq!(
        run("const r = new ShadowRealm();
             const take = r.evaluate('(fn) => { globalThis.f = fn; return typeof globalThis.f; }');
             let hits = 0;
             const t = take(() => { hits += 1; return 7; });
             const fire = r.evaluate('() => globalThis.f()');
             const out = fire();
             t + ':' + out + ':' + hits"),
        "function:7:1"
    );
    assert_eq!(
        run("globalThis.count = 0;
             const realm1 = new ShadowRealm();
             const r1wrapped = realm1.evaluate('globalThis.count = 0; () => globalThis.count += 1;');
             const realm2Evaluate = realm1.evaluate(
               'const realm2 = new ShadowRealm(); (str) => realm2.evaluate(str);'
             );
             const r2wrapper = realm2Evaluate('globalThis.wrapped = undefined; globalThis.count = 0; (fn) => globalThis.wrapped = fn;');
             r2wrapper(r1wrapped);
             const r2fire = realm2Evaluate('() => { globalThis.wrapped(); }');
             r2fire();
             const c = realm1.evaluate('globalThis.count');
             '' + c + ':' + globalThis.count"),
        "1:0"
    );
}
#[test]
fn shadow_realm_eval_scoping() {
    assert_eq!(
        run("const r2 = new ShadowRealm();
             r2.evaluate(`
               const hasOwn = Object.prototype.hasOwnProperty;
               const savedGlobal = globalThis;
               const names = Object.keys(Object.getOwnPropertyDescriptors(globalThis));
               const keep = ['undefined','Infinity','NaN'];
               const remaining = names.filter(name => {
                 if (keep.includes(name)) return false;
                 if (name !== 'globalThis') {
                   delete globalThis[name];
                   return hasOwn.call(globalThis, name);
                 }
               });
               delete globalThis['globalThis'];
               if (hasOwn.call(savedGlobal, 'globalThis')) remaining.push('globalThis');
               remaining.join(', ');
             `)"),
        ""
    );
    assert_eq!(
        run("const r = new ShadowRealm();
             r.evaluate(`
               const entries = Object.entries(Object.getOwnPropertyDescriptors(globalThis));
               entries.filter(e => e[1].configurable === false).map(([n]) => n)
                 .filter(n => !['undefined','Infinity','NaN'].includes(n)).join(', ');
             `)"),
        ""
    );
}
#[test]
fn class_constructor_call_and_return_semantics() {
    // A class constructor has no [[Call]].
    assert_eq!(throws("class C {}; C()"), "TypeError");
    // A derived constructor may only return an object or undefined.
    assert_eq!(
        run("class Base {}
             class D extends Base { constructor() { super(); return 5; } }
             try { new D(); 'no' } catch (e) { e.constructor.name }"),
        "TypeError"
    );
    // super() may only be called once.
    assert_eq!(
        run("class Base {}
             class D extends Base { constructor() { super(); super(); } }
             try { new D(); 'no' } catch (e) { e.constructor.name }"),
        "ReferenceError"
    );
    // `this` is in TDZ until super() runs.
    assert_eq!(
        run("class Base {}
             class D extends Base { constructor() { const t = this; super(); } }
             try { new D(); 'no' } catch (e) { e.constructor.name }"),
        "ReferenceError"
    );
    // Returning (even explicitly) without ever calling super() leaves `this` uninitialized.
    assert_eq!(
        run("class Base {}
             class D extends Base { constructor() { return undefined; } }
             try { new D(); 'no' } catch (e) { e.constructor.name }"),
        "ReferenceError"
    );
    // A base constructor's primitive return is ignored; an object return wins.
    assert_eq!(
        run("class B { constructor() { return 5; } } typeof new B()"),
        "object"
    );
    assert_eq!(
        run("class B { constructor() { return { x: 7 }; } } String(new B().x)"),
        "7"
    );
}

#[test]
fn date_called_as_function_returns_string() {
    assert_eq!(run("typeof Date()"), "string");
    // Date() ignores its arguments — even through a bound wrapper.
    assert_eq!(run("var b = Date.bind(null); typeof b(0,0,0)"), "string");
    // Date.prototype.toString uses the human-readable (non-ISO) format.
    assert_eq!(
        run("new Date(0).toString()"),
        "Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)"
    );
}

#[test]
fn restricted_caller_arguments_shared_accessor() {
    // getter and setter are the single %ThrowTypeError% intrinsic...
    assert_eq!(
        run(
            "var d = Object.getOwnPropertyDescriptor(Function.prototype, 'caller'); \
             var a = Object.getOwnPropertyDescriptor(Function.prototype, 'arguments'); \
             String(d.get === d.set && a.get === a.set && d.get === a.get)"
        ),
        "true"
    );
    // ...but reading it through an ordinary sloppy function reflects the stack (inactive: null),
    assert_eq!(run("function f() {} String(f.caller)"), "null");
    // while strict functions and Function.prototype itself throw.
    assert_eq!(
        throws("'use strict'; function f() {} f.caller"),
        "TypeError"
    );
    assert_eq!(throws("Function.prototype.caller"), "TypeError");
}

#[test]
fn function_to_string_source_text() {
    assert_eq!(run("({ ['a'](){ } }).a.toString()"), "['a'](){ }");
    assert_eq!(
        run("(function  foo ( a,b ) { return a; }).toString()"),
        "function  foo ( a,b ) { return a; }"
    );
    assert_eq!(run("((x)=>x+ 1).toString()"), "(x)=>x+ 1");
    assert_eq!(run("({ get  p() { return 1; } });
                    Object.getOwnPropertyDescriptor({ get  p() { return 1; } }, 'p').get.toString()"),
               "get  p() { return 1; }");
    // A class constructor stringifies as the whole class.
    assert_eq!(
        run("(class A { constructor() {} m() {} }).toString()"),
        "class A { constructor() {} m() {} }"
    );
    // Natives render as native code carrying their name; bound functions drop the
    // "bound f" compound (not a valid PropertyName).
    assert_eq!(
        run("Math.max.toString()"),
        "function max() { [native code] }"
    );
    assert_eq!(
        run("(function f(){}).bind(null).toString()"),
        "function () { [native code] }"
    );
    // Dynamic functions stringify as their synthesized source.
    assert_eq!(
        run("Function('a', 'return a').toString()"),
        "function anonymous(a\n) {\nreturn a\n}"
    );
}

#[test]
fn cross_realm_construct_semantics() {
    // GetFunctionRealm unwraps bound functions: the fallback prototype comes from the bound
    // target's realm.
    assert_eq!(
        run("const other = $262.createRealm().global;
             var nt = new other.Function(); nt.prototype = 'str';
             var bound = Function.prototype.bind.call(nt);
             var date = Reflect.construct(Date, [], bound);
             String(Object.getPrototypeOf(date) === other.Date.prototype
                    && date instanceof other.Date)"),
        "true"
    );
    // A derived constructor's return-validation TypeError is created in the CALLER's realm
    // (the callee context pops before the throw).
    assert_eq!(
        run("var C = $262.createRealm().global.eval(
                 '0, class extends Object { constructor() { return null; } }');
             try { new C(); 'no' } catch (e) { String(e.constructor === TypeError) }"),
        "true"
    );
    // A newTarget proxy revoked mid-construction (by its own `prototype` get trap) makes the
    // GetFunctionRealm fallback throw.
    assert_eq!(
        run(
            "var h = Proxy.revocable(function(){}, { get() { h.revoke(); } });
             try { new h.proxy(); 'no' } catch (e) { e.constructor.name }"
        ),
        "TypeError"
    );
}

#[test]
fn dynamic_function_coerces_params_before_body() {
    assert_eq!(
        run("var order = [];
             var p = { toString() { order.push('p'); return 'a'; } };
             var body = { toString() { order.push('b'); return 'return a;'; } };
             new Function(p, body); order.join(',')"),
        "p,b"
    );
}
#[cfg(feature = "intl")]
#[test]
fn locale_canonicalization_and_likely_subtags() {
    assert_eq!(run("new Intl.Locale('ces').toString()"), "cs");
    assert_eq!(run("new Intl.Locale('hy-arevmda').toString()"), "hyw");
    assert_eq!(
        run("new Intl.Locale('ces').maximize().toString()"),
        "cs-Latn-CZ"
    );
    // A multi-candidate territory alias (SU) resolves via likely subtags, before options apply.
    assert_eq!(
        run("new Intl.Locale('und-Armn-SU', {language: 'ru'}).toString()"),
        "ru-Armn-AM"
    );
}

#[test]
fn string_normalize_forms() {
    assert_eq!(run(r"'\u0041\u030A'.normalize('NFC')"), "\u{C5}");
    assert_eq!(run(r"'\u00C5'.normalize('NFD').length.toString()"), "2");
    assert_eq!(run(r"'\uFB01'.normalize('NFKD')"), "fi");
    assert_eq!(run("'\u{AC01}'.normalize('NFD').length.toString()"), "3");
    assert_eq!(run("'\u{1E0B}\u{323}'.normalize('NFC')"), "\u{1E0D}\u{307}");
    assert_eq!(throws("'a'.normalize('NFX')"), "RangeError");
}

#[test]
fn bigint_relational_compare_is_exact() {
    assert_eq!(
        run("String(9007199254740992000n <= 9007199254740991999n)"),
        "false"
    );
    assert_eq!(run("String(9007199254740993n > 9007199254740992)"), "true");
    assert_eq!(run("String(1n < 1.5)"), "true");
    assert_eq!(
        run("String('9007199254740992001' < 9007199254740992002n)"),
        "true"
    );
}

#[cfg(feature = "intl")]
#[test]
fn collator_three_level_compare() {
    // Case is a tertiary difference: lowercase sorts first in en.
    assert_eq!(run("String('a'.localeCompare('A'))"), "-1");
    // Canonically equivalent strings are equal.
    assert_eq!(
        run(r"String(new Intl.Collator('en').compare('o\u0308', '\u00F6'))"),
        "0"
    );
    // Accents are secondary: ä sorts between a and b.
    assert_eq!(
        run("['b','\u{E4}','a'].sort(new Intl.Collator('en').compare).join('')"),
        "a\u{E4}b"
    );
    // German phonebook expands ä to ae.
    assert_eq!(
        run("['Af','\u{C4}','Ab'].sort(new Intl.Collator('de-u-co-phonebk').compare).join(',')"),
        "Ab,\u{C4},Af"
    );
}

#[cfg(feature = "intl")]
#[test]
fn cldr_unit_patterns_correct_ids() {
    // Regression (issue #7): the CLDR table matched unit ids by bare suffix and picked up
    // unrelated compound units — `second` -> acceleration-meter-per-square-second,
    // `centimeter` -> area-square-centimeter, `minute` -> angle-arc-minute, etc.
    let unit = |u: &str, disp: &str| {
        run(&format!(
            "new Intl.NumberFormat('en',{{style:'unit',unit:'{u}',unitDisplay:'{disp}'}}).format(5)"
        ))
    };
    assert_eq!(unit("second", "long"), "5 seconds");
    assert_eq!(unit("second", "short"), "5 sec");
    assert_eq!(unit("meter", "long"), "5 meters");
    assert_eq!(unit("meter", "short"), "5 m");
    assert_eq!(unit("centimeter", "long"), "5 centimeters");
    assert_eq!(unit("minute", "long"), "5 minutes");
    assert_eq!(unit("mile", "long"), "5 miles");
    assert_eq!(unit("liter", "long"), "5 liters");
    assert_eq!(unit("gallon", "long"), "5 gallons");
    // Genuine compound speed units still resolve.
    assert_eq!(unit("kilometer-per-hour", "long"), "5 kilometers per hour");
    // DurationFormat composes the same corrected patterns.
    assert_eq!(
        run("new Intl.DurationFormat('en',{style:'long'}).format({hours:1,minutes:46,seconds:40})"),
        "1 hour, 46 minutes, 40 seconds"
    );
}
#[cfg(feature = "intl")]
#[test]
fn numberformat_exact_decimal_inputs() {
    // A BigInt beyond 2^53 keeps its exact digits.
    assert_eq!(
        run("(90071992547409910n).toLocaleString('en-US')"),
        "90,071,992,547,409,910"
    );
    // A decimal-string argument does not round through f64.
    assert_eq!(
        run("new Intl.NumberFormat('en',{useGrouping:false,maximumFractionDigits:9}).format('9007200.256743991')"),
        "9007200.256743991"
    );
}
#[cfg(feature = "intl")]
#[test]
fn dtf_chinese_calendar_year_parts() {
    assert_eq!(
        run("JSON.stringify(new Intl.DateTimeFormat('zh-u-ca-chinese',{year:'numeric'})
             .formatToParts(new Date(2019, 5, 1)))"),
        "[{\"type\":\"relatedYear\",\"value\":\"2019\"},{\"type\":\"yearName\",\"value\":\"己亥\"},{\"type\":\"literal\",\"value\":\"年\"}]"
    );
    // A DTF range with only the day differing collapses around shared fields.
    assert_eq!(
        run(
            "new Intl.DateTimeFormat('en-US',{year:'numeric',month:'short',day:'numeric'})
             .formatRange(new Date('2019-01-03T00:00:00'), new Date('2019-01-05T00:00:00'))"
        ),
        "Jan 3\u{2009}\u{2013}\u{2009}5, 2019"
    );
}
#[test]
fn regex_smuggle_range_and_vflag() {
    // U+10FFFF (a smuggle-range character) has length 2 and round-trips through v-mode classes.
    assert_eq!(run(r"'\u{10FFFF}'.length.toString()"), "2");
    assert_eq!(run(r"String(/\u{10FFFF}/v.test('\u{10FFFF}'))"), "true");
    assert_eq!(
        run(r"String(/[\u{10000}-\u{10FFFF}]/v.exec('\u{10FFFF}')[0] === '\u{10FFFF}')"),
        "true"
    );
    assert_eq!(
        run(r#"String(/\P{ASCII}/v.exec('a\u{20BB7}b'))"#),
        "\u{20BB7}"
    );
}
#[test]
fn lookbehind_backwards_matching() {
    // Lookbehind bodies match right-to-left: greed, alternative order, and captures follow.
    assert_eq!(run(r#"String('abbbbbbc'.match(/(?<=(b+))c/))"#), "c,bbbbbb");
    assert_eq!(
        run(r#"String('abcdef'.match(/(?<=(?<a>\w){3})f/u))"#),
        "f,c"
    );
    assert_eq!(run(r#"String('abcdef'.match(/(?<=(?<a>\w)+)f/u))"#), "f,a");
    assert_eq!(
        run(r#"String('abcdef'.match(/(?<=(?<a>\w){6})f/u))"#),
        "null"
    );
    assert_eq!(
        run(r#"String('ab12b23b34c'.match(/(?<=((?:b\d{2})+))c/))"#),
        "c,b12b23b34"
    );
    // Negative lookbehind discards its captures.
    assert_eq!(run(r#"String('abcdef'.match(/(?<!(?<a>\d){3})f/u))"#), "f,");
}
#[test]
fn annexb_web_compat_batch() {
    // Labelled function declarations (through label chains) in sloppy mode.
    assert_eq!(
        run("label: function g() {} label1: label2: function f() {} 'ok'"),
        "ok"
    );
    // for-in var initializer runs before the loop.
    assert_eq!(
        run("var effects = 0; var stored;
             for (var a = (++effects, -1) in stored = a, {a: 0, b: 1, c: 2}) {}
             [effects, stored, a].join('|')"),
        "1|-1|c"
    );
    // CallExpression assignment targets parse; the call runs, then ReferenceError.
    assert_eq!(
        run(
            "var called = false; function f() { called = true; return {}; }
             var r; try { f() = 1; } catch (e) { r = e.constructor.name; }
             [called, r].join('|')"
        ),
        "true|ReferenceError"
    );
    // Legacy octal / identity decimal escapes in regex literals.
    assert_eq!(run(r"String(/\1/.exec('\x01'))"), "\u{1}");
    assert_eq!(run(r"String(/(.)\1/.exec('a\x01 aa'))"), "aa,a");
    assert_eq!(run(r"String(/\0111/.exec('\x091'))"), "\u{9}1");
    assert_eq!(run(r"String(/\8/.exec('789'))"), "8");
    // $262.IsHTMLDDA emulates undefined.
    assert_eq!(
        run("var d = $262.IsHTMLDDA;
             [typeof d, !!d, d == null, d === null, String(d())].join('|')"),
        "undefined|false|true|false|null"
    );
}
#[test]
fn promise_subclass_resolver_settles_subclass_instance() {
    // The native super() grafts promise state onto the subclass `this`; a resolver captured from
    // the executor must still settle that instance (via the promise_forward redirect).
    let mut e = crate::Engine::new();
    e.eval(
        "var out='pending';
         var r;
         class C2 extends Promise { constructor(ex) { super(ex); C2.last = this; } }
         var p = new C2(function(res, rej) { r = res; });
         out = 'id:' + (p === C2.last) + ':' + (Object.getPrototypeOf(p) === C2.prototype);
         r(1);
         p.then(v => { out = 'ok:' + v; }, e => { out = 'rej:' + e; });",
        false,
    )
    .unwrap();
    match e.eval("out", false).unwrap() {
        crate::Completion::Value(v) => assert_eq!(v, "ok:1"),
        crate::Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn promise_already_resolved_is_per_resolver_pair() {
    // [[AlreadyResolved]] belongs to one resolve/reject pair: a second call on the same pair is
    // ignored, but the fresh pair created for thenable adoption must still be able to settle.
    let mut e = crate::Engine::new();
    e.eval(
        "var out = 'pending';
         var p = new Promise(function(res, rej) {
             res({ then: function(res2) { res2('adopted'); } });
             rej(new Error('ignored: pair already used'));
         });
         p.then(v => { out = 'ok:' + v; }, e => { out = 'rej:' + e; });",
        false,
    )
    .unwrap();
    match e.eval("out", false).unwrap() {
        crate::Completion::Value(v) => assert_eq!(v, "ok:adopted"),
        crate::Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn array_element_set_preserves_attributes() {
    // [[Set]] on an existing array element only updates the value; it must not replace the
    // property (which would reset enumerable/configurable to the plain defaults).
    assert_eq!(
        run("var a = [];
             Object.defineProperty(a, '0', {writable: true, enumerable: true, configurable: false});
             a[0] = 'x';
             var d = Object.getOwnPropertyDescriptor(a, '0');
             var del = delete a[0];
             [d.value, d.configurable, del, a.hasOwnProperty('0')].join('|')"),
        "x|false|false|true"
    );
}

#[test]
fn object_assign_throws_creating_on_sealed_target() {
    assert_eq!(
        run("var t = Object.seal({a: 1});
             var r;
             try { Object.assign(t, {a: 2, b: 3}); r = 'no throw'; }
             catch (e) { r = e.constructor.name + ':' + t.a + ':' + t.hasOwnProperty('b'); }
             r"),
        "TypeError:2:false"
    );
}

#[test]
fn atomics_rmw_is_atomic_across_threads() {
    // Two threads hammer Atomics.add on the same shared element; a read-modify-write that
    // releases the lock between the read and the write loses increments.
    assert_eq!(
        run("var sab = new SharedArrayBuffer(4);
             var i32a = new Int32Array(sab);
             for (var k = 0; k < 1000; k++) Atomics.add(i32a, 0, 1);
             Atomics.load(i32a, 0)"),
        "1000"
    );
}

#[test]
fn atomics_waitasync_sees_same_job_notify() {
    // waitAsync registers its waiter synchronously: a notify later in the same job wakes it.
    let mut e = crate::Engine::new();
    e.eval(
        "var out = 'pending';
         var i32a = new Int32Array(new SharedArrayBuffer(16));
         var r = Atomics.waitAsync(i32a, 0, 0);
         r.value.then(v => { out = 'v:' + v; }, e => { out = 'e:' + e; });
         Atomics.notify(i32a, 0);",
        false,
    )
    .unwrap();
    match e.eval("out", false).unwrap() {
        crate::Completion::Value(v) => assert_eq!(v, "v:ok"),
        crate::Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn super_call_early_errors() {
    // SuperCall outside a derived class constructor is a parse-time SyntaxError.
    assert!(parse_err("var C = class { constructor() { super(); } };"));
    assert!(parse_err("class C { m() { super(); } }"));
    assert!(parse_err("({ m() { super(); } });"));
    assert!(!parse_err(
        "class C extends B { constructor() { super(); } }"
    ));
    assert!(!parse_err(
        "class C extends B { constructor() { () => super(); } }"
    ));
    assert!(parse_err("class C extends B { m() { super(); } }"));
    assert!(parse_err("class C extends B { f = super(); }"));
    assert!(parse_err("class C extends B { static { super(); } }"));
    assert!(parse_err(
        "class C extends B { constructor() { function f() { super(); } } }"
    ));
}

fn parse_err(src: &str) -> bool {
    crate::Engine::new().eval(src, false).is_err()
}

#[test]
fn arrow_inherits_lexical_new_target() {
    assert_eq!(
        run("var out = [];
             function F() { out.push(typeof new.target, (_ => typeof new.target)()); }
             F();
             new F();
             out.join(',')"),
        "undefined,undefined,function,function"
    );
}

#[test]
fn private_elements_on_non_extensible_receivers() {
    // PrivateFieldAdd / PrivateMethodOrAccessorAdd throw when the receiver was made
    // non-extensible before the elements are stamped (instance and static alike).
    assert_eq!(
        run("'use strict';
             class Base { constructor(seal) { if (seal) Object.preventExtensions(this); } }
             class F extends Base { #v; constructor(s) { super(s); } }
             class M extends Base { constructor(s) { super(s); } #m() {} }
             var out = [];
             for (var K of [F, M]) {
               try { new K(true); out.push('no'); } catch (e) { out.push(e.constructor.name); }
             }
             try {
               class S { static #g = (Object.preventExtensions(S), 1); }
               out.push('no');
             } catch (e) { out.push(e.constructor.name); }
             out.join(',')"),
        "TypeError,TypeError,TypeError"
    );
}

#[test]
fn top_level_for_await_runs_outside_a_coroutine() {
    // `for await` in module top-level code has no enclosing coroutine to park; it must fall back
    // to the synchronous top-level await drive instead of panicking.
    let src = "let out = [];\nfor await (const x of [await 1, Promise.resolve(2), 3]) { out.push(x); }\nif (out.join() !== '1,2,3') throw new Error('got ' + out.join());\n";
    let mut e = Engine::new();
    match e.eval_module(src, "tla.js", |_, _| None).expect("parse") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn scratch_eval_file() {
    // Debug helper: LUMEN_SCRATCH=/path/to/file.js cargo test scratch_eval_file -- --nocapture
    if let Ok(p) = std::env::var("LUMEN_SCRATCH") {
        let src = std::fs::read_to_string(&p).expect("read scratch file");
        let mut e = Engine::new();
        let module = std::env::var("LUMEN_SCRATCH_MODULE").is_ok();
        if let Ok(pre) = std::env::var("LUMEN_SCRATCH_PRE") {
            let pre_src = std::fs::read_to_string(&pre).expect("read preamble");
            match e.eval(&pre_src, false) {
                Ok(Completion::Value(_)) => {}
                other => {
                    println!("PREAMBLE PROBLEM: {:?}", other.is_ok());
                    return;
                }
            }
        }
        let strict = std::env::var("LUMEN_SCRATCH_STRICT").is_ok();
        let r = if module {
            // Resolve relative imports against the scratch file's directory.
            let base = std::path::Path::new(&p)
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_default();
            e.eval_module(&src, &p, move |spec, _referrer| {
                let resolved = base.join(spec.trim_start_matches("./"));
                let text = std::fs::read_to_string(&resolved).ok()?;
                Some((resolved.to_string_lossy().into_owned(), text))
            })
            .expect("parse")
        } else {
            match e.eval(&src, strict) {
                Ok(c) => c,
                Err(err) => {
                    println!("PARSE ERROR: {err:?}");
                    return;
                }
            }
        };
        for line in e.take_console() {
            println!("console: {line}");
        }
        match r {
            Completion::Value(v) => println!("value: {v}"),
            Completion::Throw { name, message } => println!("throw: {name}: {message}"),
        }
    }
}

#[test]
fn debug_type_sizes() {
    if std::env::var("LUMEN_SIZES").is_err() {
        return;
    }
    println!("Stmt: {}", std::mem::size_of::<crate::ast::Stmt>());
    println!("Expr: {}", std::mem::size_of::<crate::ast::Expr>());
    println!("Token: {}", std::mem::size_of::<crate::token::Token>());
    println!("Tok: {}", std::mem::size_of::<crate::token::Tok>());
}

#[test]
fn deferred_ns_in_tla_cycle_hydrates_after_evaluation() {
    // dep defers the TLA module that imported it: reading ns.foo during evaluation is a
    // TypeError, and a read after the graph settles sees the real export (the stub is
    // hydrated lazily — at link time the base namespace was still empty).
    assert_eq!(
        run_module(
            &[
                (
                    "main",
                    r#"import "tla"; globalThis.late = globalThis.check();"#
                ),
                (
                    "tla",
                    r#"import "dep"; await Promise.resolve(); export let foo = 1;"#
                ),
                (
                    "dep",
                    r#"import defer * as ns from "tla";
                       try { void ns.foo; globalThis.early = "no-throw"; }
                       catch (e) { globalThis.early = e.constructor.name; }
                       globalThis.check = () => ns.foo;"#
                ),
            ],
            "globalThis.early + \":\" + globalThis.late"
        ),
        "TypeError:1"
    );
}

#[test]
fn dynamic_import_of_deferred_module_evaluates_it() {
    // A deferred-only dep never joins the batch: a later dynamic import must evaluate it
    // (and surface its evaluation error) instead of waiting on an orphan promise forever.
    assert_eq!(
        run_module(
            &[
                (
                    "main",
                    r#"import defer * as ns from "boom";
                       import("boom").catch(e => {
                         globalThis.err1 = e.someError;
                         try { void ns.x; } catch (e2) { globalThis.same = e2 === e; }
                       });"#
                ),
                ("boom", r#"throw { someError: "from boom" };"#),
            ],
            "globalThis.err1 + \":\" + globalThis.same"
        ),
        "from boom:true"
    );
}

#[test]
fn tla_fulfillment_resolves_leaf_before_ancestors() {
    // AsyncModuleExecutionFulfilled step 7: the fulfilled module's own promise resolves
    // before available ancestors execute, so import(b) settles before import(a) even though
    // a's reaction was registered first.
    assert_eq!(
        run_module(
            &[
                (
                    "main",
                    r#"globalThis.logs = [];
                       import("a").then(() => globalThis.logs.push("A"));
                       import("b").then(() => globalThis.logs.push("B"));"#
                ),
                ("a", r#"import "b";"#),
                ("b", r#"await Promise.resolve();"#),
            ],
            "globalThis.logs.join(\",\")"
        ),
        "B,A"
    );
}

#[test]
fn dynamic_import_does_not_preempt_dfs_order() {
    // A dynamic import of a later sibling in an in-flight Evaluate() waits for the batch's
    // DFS to reach it instead of executing it early.
    assert_eq!(
        run_module(
            &[
                ("main", r#"import "a"; import "b";"#),
                (
                    "a",
                    r#"globalThis.logs = [];
                       import("b").then(() => globalThis.logs.push("dyn"));
                       globalThis.logs.push("A");"#
                ),
                ("b", r#"globalThis.logs.push("B");"#),
            ],
            "globalThis.logs.join(\",\")"
        ),
        "A,B,dyn"
    );
}

// ---- import attributes: `with { type: "text" }` (TC39 proposal-import-text, stage 3) ----

#[test]
fn import_text_modules() {
    // A text module default-exports the file contents verbatim (CreateTextModule): no parsing,
    // no execution — importing a .js file as text must NOT run it. The namespace has exactly
    // `default`, and a dynamic import with the same attribute resolves to the same record
    // (keyed `path#text`, distinct from any ordinary module of the file).
    let mut files: std::collections::HashMap<String, String> = Default::default();
    files.insert("/note.txt".into(), "hello text\nline 2 \u{e9}".into());
    files.insert(
        "/mod.js".into(),
        "globalThis.__executed = true; export default 1;".into(),
    );
    files.insert(
        "/main.js".into(),
        r#"
        import note from '/note.txt' with { type: 'text' };
        import js from '/mod.js' with { type: 'text' };
        import * as ns from '/note.txt' with { type: 'text' };
        globalThis.__note = note;
        globalThis.__js_is_source =
            js === "globalThis.__executed = true; export default 1;";
        globalThis.__not_executed = typeof globalThis.__executed === 'undefined';
        globalThis.__ns =
            Object.getOwnPropertyNames(ns).join(',') + ':' + (ns.default === note);
        import('/note.txt', { with: { type: 'text' } }).then(m => {
            globalThis.__dyn_same = m.default === note;
        });
        "#
        .into(),
    );
    let f = files.clone();
    let mut e = Engine::new();
    e.eval_module_attrs(
        &f["/main.js"].clone(),
        "/main.js",
        move |spec, _r, _attr| f.get(spec).map(|s| (spec.to_string(), s.clone())),
    )
    .unwrap();
    let read = |e: &mut Engine, src: &str| match e.eval(src, false).unwrap() {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("{src} threw {name}: {message}"),
    };
    assert_eq!(
        read(&mut e, "globalThis.__note"),
        "hello text\nline 2 \u{e9}"
    );
    assert_eq!(read(&mut e, "globalThis.__js_is_source"), "true");
    assert_eq!(read(&mut e, "globalThis.__not_executed"), "true");
    assert_eq!(read(&mut e, "globalThis.__ns"), "default:true");
    assert_eq!(read(&mut e, "globalThis.__dyn_same"), "true");
}

#[test]
fn import_text_attribute_reaches_loader() {
    // The loader receives the `with { type: ... }` attribute, so a host can serve raw contents
    // for attribute imports while serving executable source for ordinary ones — of the same
    // specifier, in the same graph.
    let mut e = Engine::new();
    e.eval_module_attrs(
        r#"
        import ordinary from '/dual.js';
        import astext from '/dual.js' with { type: 'text' };
        globalThis.__r = ordinary + ':' + astext;
        "#,
        "/main.js",
        |spec, _r, attr| match (spec, attr) {
            ("/dual.js", None) => Some((spec.to_string(), "export default 'ran';".to_string())),
            ("/dual.js", Some("text")) => Some((spec.to_string(), "RAW".to_string())),
            _ => None,
        },
    )
    .unwrap();
    match e.eval("globalThis.__r", false).unwrap() {
        Completion::Value(v) => assert_eq!(v, "ran:RAW"),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

// ---- import attributes: `with { type: "bytes" }` (TC39 proposal-import-bytes) ----

#[test]
fn import_bytes_modules() {
    // A bytes module default-exports a `Uint8Array` over an *immutable* buffer, byte-exact for
    // arbitrary binary content. The loader hands binary over latin-1-decoded (one char per
    // byte); the engine re-extracts the original bytes. Writes through the view fail like a
    // non-writable property: TypeError in strict (module) code; resize/transfer throw.
    let blob: String = [0u8, 1, 0xfe, 0xff, 0x80, 65]
        .iter()
        .map(|&b| b as char)
        .collect();
    let mut files: std::collections::HashMap<String, String> = Default::default();
    files.insert("/blob.bin".into(), blob);
    files.insert(
        "/main.js".into(),
        r#"
        import b from '/blob.bin' with { type: 'bytes' };
        globalThis.__b = b;
        globalThis.__shape = [
            b instanceof Uint8Array,
            b.length === 6,
            Array.from(b).join(','),
            b.buffer.immutable === true,
        ].join('|');
        let wrote = 'no-throw';
        try { b[0] = 9; } catch (e) { wrote = e.constructor.name; }
        globalThis.__strict_write = wrote + ':' + b[0];
        let resized = 'no-throw';
        try { b.buffer.resize(1); } catch (e) { resized = e.constructor.name; }
        globalThis.__resize = resized;
        "#
        .into(),
    );
    let f = files.clone();
    let mut e = Engine::new();
    e.eval_module_attrs(
        &f["/main.js"].clone(),
        "/main.js",
        move |spec, _r, _attr| f.get(spec).map(|s| (spec.to_string(), s.clone())),
    )
    .unwrap();
    let read = |e: &mut Engine, src: &str| match e.eval(src, false).unwrap() {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("{src} threw {name}: {message}"),
    };
    assert_eq!(
        read(&mut e, "globalThis.__shape"),
        "true|true|0,1,254,255,128,65|true"
    );
    assert_eq!(read(&mut e, "globalThis.__strict_write"), "TypeError:0");
    assert_eq!(read(&mut e, "globalThis.__resize"), "TypeError");
    // Sloppy-mode writes over an immutable buffer are a SILENT no-op (spec: the [[Set]] just
    // returns false), still after the observable value coercion.
    assert_eq!(
        read(
            &mut e,
            "globalThis.__b[0] = 9; String(globalThis.__b[0]) + ':' + (globalThis.__b[5] = 7, globalThis.__b[5])"
        ),
        "0:65"
    );
}

// ---- import attributes: `with { type: "json" }` (JSON modules) ----

#[test]
fn import_json_modules() {
    // A JSON module default-exports the JSON.parse of its source: `__proto__` keys become plain
    // own data properties (never prototype-setting), the value is mutable (not frozen), the
    // namespace has exactly `default`, and a plain import of the same specifier is a DIFFERENT
    // record from the attribute import. Dynamic import with the attribute dedups to the same
    // record; invalid JSON surfaces as a SyntaxError.
    let mut files: std::collections::HashMap<String, String> = Default::default();
    files.insert(
        "/d.json".into(),
        r#"{ "answer": 42, "__proto__": { "evil": true }, "arr": [1, 2] }"#.into(),
    );
    files.insert("/bad.json".into(), "{ bad".into());
    files.insert(
        "/main.js".into(),
        r#"
        import data from '/d.json' with { type: 'json' };
        import * as ns from '/d.json' with { type: 'json' };
        globalThis.__data = data;
        globalThis.__value = data.answer + ':' + data.arr.length;
        globalThis.__proto_safe = [
            Object.getPrototypeOf(data) === Object.prototype,
            data.evil === undefined,
            Object.getOwnPropertyNames(data).includes('__proto__'),
        ].join('|');
        data.answer = 43; // JSON module values are ordinary mutable objects
        globalThis.__mutable = data.answer === 43 && !Object.isFrozen(data);
        globalThis.__ns = Object.getOwnPropertyNames(ns).join(',') + ':' + (ns.default === data);
        import('/d.json', { with: { type: 'json' } }).then(m => {
            globalThis.__dyn_same = m.default === data;
        });
        import('/bad.json', { with: { type: 'json' } }).then(
            () => { globalThis.__bad = 'resolved'; },
            e => { globalThis.__bad = e.constructor.name; },
        );
        "#
        .into(),
    );
    let f = files.clone();
    let mut e = Engine::new();
    e.eval_module_attrs(
        &f["/main.js"].clone(),
        "/main.js",
        move |spec, _r, _attr| f.get(spec).map(|s| (spec.to_string(), s.clone())),
    )
    .unwrap();
    let read = |e: &mut Engine, src: &str| match e.eval(src, false).unwrap() {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("{src} threw {name}: {message}"),
    };
    assert_eq!(read(&mut e, "globalThis.__value"), "42:2");
    assert_eq!(read(&mut e, "globalThis.__proto_safe"), "true|true|true");
    assert_eq!(read(&mut e, "globalThis.__mutable"), "true");
    assert_eq!(read(&mut e, "globalThis.__ns"), "default:true");
    assert_eq!(read(&mut e, "globalThis.__dyn_same"), "true");
    assert_eq!(read(&mut e, "globalThis.__bad"), "SyntaxError");
}

#[test]
fn import_json_attr_distinct_from_plain_import() {
    // The same specifier imported with and without the attribute resolves two records: the
    // attribute one is engine-synthesized from raw contents, the plain one is whatever module
    // the host serves. Order must not matter (regression: the dep map used to key by specifier
    // alone, collapsing both onto whichever import came first).
    for flipped in [false, true] {
        let a = "import j from '/d.json' with { type: 'json' };";
        let b = "import p from '/d.json';";
        let (first, second) = if flipped { (b, a) } else { (a, b) };
        let src = format!("{first}\n{second}\nglobalThis.__r = (j === p) + ':' + j.k + ':' + p.k;");
        let mut e = Engine::new();
        e.eval_module_attrs(&src, "/main.js", |spec, _r, attr| match (spec, attr) {
            ("/d.json", Some("json")) => Some((spec.to_string(), r#"{"k":"raw"}"#.to_string())),
            ("/d.json", None) => Some((
                spec.to_string(),
                "export default { k: 'module' };".to_string(),
            )),
            _ => None,
        })
        .unwrap();
        match e.eval("globalThis.__r", false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "false:raw:module", "flipped={flipped}"),
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Loop-spanning JIT chains (aarch64-macos): fully-chainable loops keep locals in registers
// across the back edge. These pin the guard/bail/flush semantics on the machine-code tier;
// elsewhere they still pass (the plain tiers run the same programs).
// ---------------------------------------------------------------------------------------------

fn run_jit(src: &str) -> String {
    let mut e = Engine::new();
    e.set_tier(crate::bytecode::Tier::Jit);
    e.set_tier_threshold(0);
    match e.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

#[test]
fn jit_moved_frames_preserve_activations_and_arguments() {
    // Hot environment-bearing calls move their owned arguments into the fixed JIT frame after
    // seeding captured bindings. Escaped closures must keep that activation alive, lexical
    // `this` must see the bound method receiver, and an `arguments` object must include surplus
    // arguments even though those source stack values are consumed by the moved entry.
    assert_eq!(
        run_jit(
            "function make(x) {
               return function step(y) { x = x + y; return x; };
             }
             function makeArrow(x) {
               return (y) => this.base + x + y;
             }
             function args(a) {
               return arguments.length + ':' + arguments[0] + ':' + arguments[2];
             }
             function hot(n) {
               var sum = 0, keep;
               for (var i = 0; i < n; i++) {
                 keep = make(i);
                 sum = sum + keep(1) + keep(2);
               }
               var obj = { base: 40, makeArrow: makeArrow };
               var arrow = obj.makeArrow(2);
               return sum + ':' + keep(3) + ':' + arrow(5) + ':' + args(7, 8, 9);
             }
             var out;
             for (var r = 0; r < 80; r++) out = hot(40);
             out"
        ),
        "1720:45:47:3:7:9"
    );
}

#[test]
fn loop_chain_int_kernel() {
    // bignum-style inner loop: elem reads/writes, masks, shifts, int mul/add chains.
    assert_eq!(
        run_jit(
            "function kern(src, dst, x, n) {
               var xl = x & 0x3fff, xh = x >> 14, i = 0, j = 0, c = 0;
               while (--n >= 0) {
                 var l = src[i] & 0x3fff;
                 var h = src[i++] >> 14;
                 var m = xh * l + h * xl;
                 l = xl * l + ((m & 0x3fff) << 14) + dst[j] + c;
                 c = (l >> 28) + (m >> 14) + xh * h;
                 dst[j++] = l & 0xfffffff;
               }
               return c;
             }
             var a = [], b = [];
             for (var k = 0; k < 40; k++) { a[k] = (k * 2654435 + 7) & 0xfffffff; b[k] = 0; }
             var c = 0;
             for (var r = 0; r < 30; r++) c = kern(a, b, 123456789 & 0xfffffff, 40);
             c + ':' + b[7] + ':' + b[39]"
        ),
        "47611497:91409489:39701699"
    );
}

#[test]
fn loop_chain_name_probe_does_not_clobber_sixth_integer_home() {
    // The name IC probe uses x7 as its packed/wide marker. A region with six integer-resident
    // locals also assigns x7, so captured/global names must be validated before local homes are
    // populated. This four-receiver stencil is the pressure shape that exposed the overwrite.
    assert_eq!(
        run_jit(
            "var width = 6, rowSize = 8;
             function project(u, v, p, div, h, j) {
               var row = j * rowSize;
               var previousRow = (j - 1) * rowSize;
               var prevValue = row - 1;
               var currentRow = row;
               var nextValue = row + 1;
               var nextRow = (j + 1) * rowSize;
               for (var i = 1; i <= width; i++) {
                 div[++currentRow] =
                   h * (u[++nextValue] - u[++prevValue] +
                        v[++nextRow] - v[++previousRow]);
                 p[currentRow] = 0;
               }
             }
             var u = [], v = [], p = [], div = [];
             for (var k = 0; k < 100; k++) {
               u[k] = k * 0.25 + 1;
               v[k] = k * -0.125 + 3;
               p[k] = 9;
               div[k] = 7;
             }
             for (var r = 0; r < 40; r++) project(u, v, p, div, -0.1, 3);
             var out = [];
             for (var n = 24; n <= 30; n++) out.push(div[n] + ':' + p[n]);
             out.join('|')"
        ),
        "7:9|0.15000000000000002:0|0.15000000000000002:0|0.15000000000000002:0|0.15000000000000002:0|0.15000000000000002:0|0.15000000000000002:0"
    );
}

#[test]
fn loop_chain_zero_trip_and_bails() {
    // Zero-trip: virgin locals keep their pre-loop values (nothing sanitized or flushed).
    assert_eq!(
        run_jit(
            "function f(n) {
               var s = 'keep';
               var arr = [1, 2, 3];
               var i = 0, t = 0;
               while (--n >= 0) { t = arr[i] & 3; i++; s = 1; }
               return s + ':' + t + ':' + i;
             }
             f(5); f(0) + '|' + f(-3) + '|' + f(2)"
        ),
        "keep:0:0|keep:0:0|1:2:2"
    );
    // A hole bails mid-iteration; the plain templates finish with identical state.
    assert_eq!(
        run_jit(
            "function f(arr, n) {
               var s = 0, i = 0;
               while (--n >= 0) { s = s + (arr[i] & 0xff); i++; }
               return s;
             }
             var good = [1, 2, 3, 4, 5, 6, 7, 8];
             for (var r = 0; r < 40; r++) f(good, 8);
             var holey = [1, 2, , 4, 5];
             f(holey, 5) + ':' + f(good, 8) + ':' + f([1.5, 2, 3.25, 4], 4)"
        ),
        "12:36:10"
    );
}

#[test]
fn loop_chain_counter_edges() {
    // i32 overflow in a ++ counter bails to the plain loop and stays exact.
    assert_eq!(
        run_jit(
            "function f(i, n) {
               var s = 0;
               while (--n >= 0) { s = (s + i) % 97; i = i + 1; }
               return s + ':' + i;
             }
             for (var r = 0; r < 40; r++) f(5, 10);
             f(2147483640, 20)"
        ),
        "89:2147483660"
    );
    // Walking past 2^53 must stick like f64 (the plain tier's semantics), not keep counting.
    assert_eq!(
        run_jit(
            "function f(i, n) {
               var last = 0;
               while (--n >= 0) { i = i + 1; last = i; }
               return last;
             }
             for (var r = 0; r < 40; r++) f(3, 10);
             f(9007199254740989, 6)"
        ),
        "9007199254740992"
    );
}

#[test]
fn loop_chain_float_loops_stay_float() {
    // A float kernel must not be sent through int entry guards (it would bail every entry).
    assert_eq!(
        run_jit(
            "function f(arr, n) {
               var s = 0.0, i = 0;
               while (--n >= 0) { s = s + arr[i] * 1.5; i++; }
               return s;
             }
             var a = [0.5, 1.25, 2.75, 3.125, 4.0625];
             for (var r = 0; r < 40; r++) f(a, 5);
             f(a, 5)"
        ),
        "17.53125"
    );
}

#[test]
fn loop_chain_elem_dedup_and_aliasing() {
    // src and dst are the same array: the element-read memo must not survive the write.
    assert_eq!(
        run_jit(
            "function f(a, b, n) {
               var i = 0, s = 0;
               while (--n >= 0) { s = s + (a[i] & 0xff); b[i] = (a[i] & 0xf) + 1; s = s + (a[i] & 0xff); i++; }
               return s;
             }
             var x = [10, 20, 30, 40, 50, 60];
             for (var r = 0; r < 40; r++) { var y = [0,0,0,0,0,0]; f(x, y, 6); }
             f(x, x, 6)"
        ),
        "266"
    );
}

#[test]
fn jit_bitnot_numeric_fast_path_and_coercion_bails() {
    // Exercise signed boundaries, modulo-2^32 behavior, fractional truncation and the values
    // that must bail out of the machine template to full ToNumber/ToInt32 semantics.
    assert_eq!(
        run_jit(
            "function f(x) { return ~x; }
             var hot = 0;
             for (var i = 0; i < 200; i++) hot = f(i);
             [f(0), f(-1), f(2147483647), f(2147483648),
              f(4294967295), f(4294967296), f(3.9), f(-3.9),
              f(NaN), f(Infinity), f(-Infinity), f('7'),
              f({ valueOf: function () { return 9; } })].join(':')"
        ),
        "-1:0:-2147483648:2147483647:0:-1:-4:2:-1:-1:-1:-8:-10"
    );
    assert_eq!(
        run_jit(
            "function f(x) { return ~x; }
             for (var i = 0; i < 100; i++) f(i);
             String(f(1n))"
        ),
        "-2"
    );
    assert_eq!(
        run_jit(
            "function f(x) { return ~x; }
             for (var i = 0; i < 100; i++) f(i);
             try { f(Symbol('x')); 'no throw' } catch (e) { e.name }"
        ),
        "TypeError"
    );
}

#[test]
fn jit_plain_object_templates_move_values_without_aliasing() {
    // Repeated literal sites must retain independent descriptors and owned refcounted values.
    // Numeric-looking keys also exercise the template's dense lookup sidecar copy.
    assert_eq!(
        run_jit(
            "function make(i) {
               var child = { value: i };
               return { alpha: 'v' + i, child: child, 0: i + 10, omega: [i] };
             }
             var first = make(1), last, checksum = 0;
             for (var i = 0; i < 2000; i++) {
               last = make(i);
               checksum += last.child.value + last[0] + last.omega[0];
             }
             last.alpha = 'changed'; last.child.value = 99; last.omega[0] = 88;
             [checksum, first.alpha, first.child.value, first[0], first.omega[0],
              last.alpha, last.child.value, last.omega[0], Object.keys(first).join(',')].join(':')"
        ),
        "6017000:v1:1:11:1:changed:99:88:0,alpha,child,omega"
    );
}

#[test]
fn jit_direct_calls_support_wide_argument_lists() {
    // More than eight arguments used to force every hot call through the layered Rust path.
    // Keep refcounted operands, method receivers, nested wide calls and an unwind in the test:
    // these pin the move/drop ownership rules on both successful and throwing exits.
    assert_eq!(
        run_jit(
            "function sum12(a,b,c,d,e,f,g,h,i,j,k,l) {
               return a+b+c+d+e+f+g+h+i+j+k+l;
             }
             function wrap12(a,b,c,d,e,f,g,h,i,j,k,l) {
               return sum12(a,b,c,d,e,f,g,h,i,j,k,l);
             }
             var obj = {
               base: 100,
               join12: function(a,b,c,d,e,f,g,h,i,j,k,l) {
                 if (a === 'throw') throw new Error(k.tag);
                 return this.base + ':' + a.tag+b.tag+c.tag+d.tag+e.tag+f.tag+
                        g.tag+h.tag+i.tag+j.tag+k.tag+l.tag;
               }
             };
             var nums = 0, text = '';
             for (var r = 0; r < 500; r++) {
               nums = wrap12(1,2,3,4,5,6,7,8,9,10,11,12);
               text = obj.join12({tag:'a'},{tag:'b'},{tag:'c'},{tag:'d'},
                                 {tag:'e'},{tag:'f'},{tag:'g'},{tag:'h'},
                                 {tag:'i'},{tag:'j'},{tag:'k'},{tag:'l'});
             }
             var caught;
             try { obj.join12('throw',{tag:'b'},{tag:'c'},{tag:'d'},
                              {tag:'e'},{tag:'f'},{tag:'g'},{tag:'h'},
                              {tag:'i'},{tag:'j'},{tag:'boom'},{tag:'l'}); }
             catch (e) { caught = e.message; }
             nums + '|' + text + '|' + caught"
        ),
        "78|100:abcdefghijkl|boom"
    );
}

#[test]
fn jit_slice_and_hasown_intrinsics_preserve_slow_paths() {
    assert_eq!(
        run_jit(
            "function cut(s, a, b) { return s.slice(a, b); }
             var out = '';
             for (var i = 0; i < 400; i++) out = cut('abcdefghij', 2, 7);
             var coerced = 0;
             var bound = { valueOf: function () { coerced++; return 3; } };
             [out, cut('abcdefghij', -4, 99), cut('abcdefghij', NaN, 2),
              cut('åbcdef', 1, 4), cut('abcdef', bound, 5), coerced].join(':')"
        ),
        "cdefg:ghij:ab:bcd:de:1"
    );
    assert_eq!(
        run_jit(
            "function own(o, k) { return Object.hasOwn(o, k); }
             var o = { alpha: 1, beta: 2 };
             var v;
             for (var i = 0; i < 400; i++) v = own(o, i & 1 ? 'alpha' : 'missing');
             var sym = Symbol('s'); o[sym] = 3;
             var before = own(o, 'alpha') + ':' + own(o, 'missing') + ':' + own(o, sym);
             delete o.alpha;
             var deleted = own(o, 'alpha');
             o.alpha = 4;
             var restored = own(o, 'alpha');
             var saved = Object.hasOwn;
             Object.hasOwn = function () { return 'changed'; };
             var changed = own(o, 'alpha');
             Object.hasOwn = saved;
             var thrown;
             try { own(1, 'x'); } catch (e) { thrown = e.name; }
             before + ':' + deleted + ':' + restored + ':' + changed + ':' + thrown"
        ),
        "true:false:true:false:true:changed:TypeError"
    );
}

// ---------------------------------------------------------------------------------------------
// Speculative inlining: hot chunks recompile with monomorphic callees spliced inline behind an
// identity guard (bytecode::plan_inlines). Drivers loop enough times to cross the recompile
// trigger; every case must behave exactly like the generic call path.
// ---------------------------------------------------------------------------------------------

#[test]
fn inline_four_way_nested_dispatch_and_deopt() {
    assert_eq!(
        run_jit(
            "function A() {} function B() {} function C() {} function D() {}
             A.prototype.bump = function (x) { return x + 1; };
             B.prototype.bump = function (x) { return x + 2; };
             C.prototype.bump = function (x) { return x + 3; };
             D.prototype.bump = function (x) { return x + 4; };
             A.prototype.run = function (x) { return this.bump(x); };
             B.prototype.run = function (x) { return this.bump(x); };
             C.prototype.run = function (x) { return this.bump(x); };
             D.prototype.run = function (x) { return this.bump(x); };
             var xs = [new A(), new B(), new C(), new D()];
             function dispatch(xs, n) {
               var sum = 0;
               for (var i = 0; i < n; i++) sum += xs[i & 3].run(i);
               return sum;
             }
             for (var r = 0; r < 300; r++) dispatch(xs, 8);
             var before = dispatch(xs, 8);
             B.prototype.run = function (x) { return x * 10; };
             before + ':' + dispatch(xs, 8)"
        ),
        "48:98"
    );
}

#[test]
fn inline_deopt_on_method_reassignment() {
    assert_eq!(
        run_jit(
            "function A() {}
             A.prototype.m = function (x) { return x + 1; };
             var a = new A();
             function driver(a, i) { return a.m(i); }
             var s = 0;
             for (var i = 0; i < 500; i++) s += driver(a, i);
             A.prototype.m = function (x) { return x * 1000; };
             for (var i = 0; i < 10; i++) s += driver(a, i);
             s"
        ),
        "170250"
    );
}

#[test]
fn inline_vars_reset_per_invocation() {
    assert_eq!(
        run_jit(
            "function acc(n) {
               var t;
               if (n > 0) t = n;
               return typeof t;
             }
             var o = { acc: acc };
             function driver(o, n) { return o.acc(n); }
             for (var i = 0; i < 300; i++) driver(o, 1);
             driver(o, 1) + ':' + driver(o, 0)"
        ),
        "number:undefined"
    );
}

#[test]
fn inline_argc_adjustment_and_returns() {
    assert_eq!(
        run_jit(
            "function f(a, b, c) { return '' + a + b + c; }
             var o = { f: f };
             function d2(o) { return o.f(1, 2); }
             function d5(o) { return o.f(1, 2, 3, 4, 5); }
             for (var i = 0; i < 300; i++) { d2(o); d5(o); }
             d2(o) + '|' + d5(o)"
        ),
        "12undefined|123"
    );
    assert_eq!(
        run_jit(
            "function find(arr, x) {
               for (var i = 0; i < arr.length; i++) {
                 if (arr[i] === x) return i;
               }
               return -1;
             }
             var o = { find: find };
             var arr = [3, 1, 4, 1, 5, 9, 2, 6];
             function driver(o, x) { return o.find(arr, x); }
             var s = 0;
             for (var i = 0; i < 400; i++) s += driver(o, i & 7);
             s + ':' + driver(o, 9) + ':' + driver(o, 42)"
        ),
        "900:5:-1"
    );
}

#[test]
fn inline_sloppy_this_primitive_receiver_deopts() {
    assert_eq!(
        run_jit(
            "function who() { return typeof this; }
             Number.prototype.who = who;
             function driver(o) { return o.who(); }
             var obj = { who: who };
             for (var i = 0; i < 300; i++) driver(obj);
             driver(obj) + ':' + driver(5)"
        ),
        "object:object"
    );
}

#[test]
fn inline_throw_from_spliced_body() {
    assert_eq!(
        run_jit(
            "function pick(arr, i) { return arr[i].x; }
             var o = { pick: pick };
             var arr = [{ x: 1 }, { x: 2 }];
             function driver(o, i) { return o.pick(arr, i); }
             var s = 0;
             for (var i = 0; i < 300; i++) s += driver(o, i & 1);
             var caught = '';
             try { driver(o, 7); } catch (e) { caught = e instanceof TypeError; }
             s + ':' + caught"
        ),
        "450:true"
    );
}

#[test]
fn inline_recompile_preserves_monomorphic_and_polymorphic_property_sites() {
    // The second-stage compiler seeds property ICs from the hot source chunks. Own-field reads
    // should remain monomorphic after splicing, while the shared virtual-call site must retain
    // every observed receiver shape instead of baking only its most recent way.
    assert_eq!(
        run_jit(
            "function A(x) { this.x = x; }
             function B(x) { this.x = x; this.pad = 1; }
             function C(x) { this.x = x; this.pad = 1; this.more = 2; }
             A.prototype.run = function(n) { return this.x + n; };
             B.prototype.run = function(n) { return this.x - n; };
             C.prototype.run = function(n) { return this.x * n; };
             function dispatch(task, n) { return task.run(n); }
             function mono(task, n) { return task.x + n; }
             var tasks = [new A(10), new B(20), new C(3)];
             var sum = 0;
             for (var i = 0; i < 600; i++) {
               sum += dispatch(tasks[i % 3], 2);
               sum += mono(tasks[0], 1);
             }
             sum"
        ),
        "13800"
    );
}

#[test]
fn inline_recompile_preserves_four_way_call_sites_across_epoch_refill() {
    // `dispatch`'s second-stage compile guards all four observed method identities and must also
    // carry the four-way CallIc profile into its generic guard tail.  A fifth identity takes that
    // tail, then an unrelated inline compile bumps CALL_IC_EPOCH.  The copied entry must
    // miss/refill at the new epoch and execute the replacement exactly once per invocation.
    assert_eq!(
        run_jit(
            "function f0(x) { return x + 1; }
             function f1(x) { return x + 2; }
             function f2(x) { return x + 3; }
             function f3(x) { return x + 4; }
             var tasks = [{ run: f0 }, { run: f1 }, { run: f2 }, { run: f3 }];
             function dispatch(task, x) { return task.run(x); }
             function outer(task, x) { return dispatch(task, x); }
             for (var i = 0; i < 800; i++) outer(tasks[i & 3], i & 7);

             function check(value, message) {
               if (!value) throw 'seeded call cache: ' + message;
             }
             check([outer(tasks[0], 10), outer(tasks[1], 10),
                    outer(tasks[2], 10), outer(tasks[3], 10)].join(',') ===
                   '11,12,13,14', 'four inherited ways');

             var replacementHits = 0;
             tasks[1].run = function (x) { replacementHits++; return x * 10; };
             check(outer(tasks[1], 7) === 70 && replacementHits === 1,
                   'fifth callee deopt');

             // Compiling this independent caller produces another second-stage chunk and bumps
             // the process-wide call epoch after the replacement entry above was filled.
             function epochLeaf(x) { return x + 100; }
             function epochDriver(x) { return epochLeaf(x); }
             function epochOuter(x) { return epochDriver(x); }
             for (var i = 0; i < 400; i++) epochOuter(i);

             check(outer(tasks[1], 8) === 80 && replacementHits === 2,
                   'epoch miss refilled once');
             check(outer(tasks[1], 9) === 90 && replacementHits === 3,
                   'refilled replacement hit');
             var secondHits = 0;
             tasks[3].run = function (x) { secondHits++; return x * 100; };
             check(outer(tasks[3], 6) === 600 && secondHits === 1,
                   'post-epoch method mutation');
             check(outer(tasks[0], 5) === 6 && outer(tasks[2], 5) === 8,
                   'original inline ways remain live');
             'ok'"
        ),
        "ok"
    );
}

#[test]
fn inline_seeded_call_cache_pins_dead_callee_addresses() {
    // The four dynamic targets contain handlers, so they cannot be spliced.  The small `leaf`
    // call still causes `dispatch` to receive a second-stage compile, where its generic function
    // call inherits all four CallIc entries and their Weak address pins.  After an epoch refill,
    // drop every target and allocate many fresh closures: no recycled address may turn a new
    // function into a stale identity hit (the classic raw-pointer ABA failure).
    assert_eq!(
        run_jit(
            "var OLD_HITS = [0, 0, 0, 0];
             var oldFns = [
               Function('x', 'OLD_HITS[0]++; try { return x + 1; } catch (e) { return -1; }'),
               Function('x', 'OLD_HITS[1]++; try { return x + 2; } catch (e) { return -1; }'),
               Function('x', 'OLD_HITS[2]++; try { return x + 3; } catch (e) { return -1; }'),
               Function('x', 'OLD_HITS[3]++; try { return x + 4; } catch (e) { return -1; }')
             ];
             function leaf(x) { return x * 2; }
             function dispatch(fn, x) { return leaf(fn(x)); }
             function outer(fn, x) { return dispatch(fn, x); }
             for (var i = 0; i < 800; i++) outer(oldFns[i & 3], i & 7);

             function epochLeaf(x) { return x - 1; }
             function epochDriver(x) { return epochLeaf(x); }
             function epochOuter(x) { return epochDriver(x); }
             for (var i = 0; i < 400; i++) epochOuter(i);
             for (var i = 0; i < 4; i++) {
               if (outer(oldFns[i], 10) !== (11 + i) * 2)
                 throw 'dead call seed: stale epoch refill ' + i;
             }
             if (OLD_HITS.join(',') !== '201,201,201,201')
               throw 'dead call seed: wrong old hit counts ' + OLD_HITS.join(',');

             oldFns = null;
             function makeFresh(k) { return function (x) { return k + x; }; }
             var churn = [];
             for (var i = 0; i < 512; i++) churn[i] = makeFresh(1000 + i);
             for (var i = 0; i < churn.length; i++) {
               if (outer(churn[i], 1) !== (1001 + i) * 2)
                 throw 'dead call seed: recycled callee ' + i;
             }
             var replacementHits = 0;
             var replacement = function (x) { replacementHits++; return x * 3; };
             if (outer(replacement, 4) !== 24 ||
                 outer(replacement, 5) !== 30 || replacementHits !== 2)
               throw 'dead call seed: final refill';
             'ok'"
        ),
        "ok"
    );
}

#[test]
fn jit_peek_truthiness_covers_all_value_kinds() {
    // `&&`, `||`, and `??` keep the tested value on the operand stack. Their ARM64 path checks
    // common tags without taking ownership; BigInt and HTMLDDA deliberately exercise the helper
    // fallback while nullish coalescing must still treat HTMLDDA as a non-nullish object.
    assert_eq!(
        run_jit(
            "function flags(v) {
               return (v ? 100 : 0) + ((v && true) ? 10 : 0) +
                      (((v ?? null) === null) ? 0 : 1);
             }
             var values = [undefined, null, false, true, 0, NaN, 1, '', 'x',
                           Symbol(), {}, 0n, 1n, $262.IsHTMLDDA];
             for (var r = 0; r < 300; r++) {
               for (var i = 0; i < values.length; i++) flags(values[i]);
             }
             values.map(flags).join(',')"
        ),
        "0,0,1,111,1,1,111,1,111,111,111,1,111,1"
    );
}

#[test]
fn jit_local_equality_branch_preserves_coercion_and_htmldda() {
    // The local/local branch fusion handles borrowed object identity and nullish values. Mixed
    // coercing pairs, TDZ, and the HTMLDDA nullish exception must retain the checked helpers.
    assert_eq!(
        run_jit(
            "function ne(a,b){if(a!=b)return 1;return 0;}
             function eq(a,b){if(a==b)return 1;return 0;}
             function sne(a,b){if(a!==b)return 1;return 0;}
             function seq(a,b){if(a===b)return 1;return 0;}
             var a={}, b={};
             for(var i=0;i<600;i++){
               ne(a,null); ne(a,b); eq(a,a); sne(a,null); seq(a,a);
             }
             var coercions=0;
             var c={valueOf:function(){coercions++;return 7;}};
             [ne(a,a),ne(a,b),ne(a,null),ne(null,undefined),ne(null,0),
              ne($262.IsHTMLDDA,null),ne(c,7),coercions,
              eq(a,a),eq(a,b),eq($262.IsHTMLDDA,null),
              sne(a,a),sne(a,b),sne(null,undefined),
              seq(a,a),seq(a,b),seq(null,undefined)].join(':')"
        ),
        "0:1:1:0:1:0:0:1:1:0:1:0:1:1:1:0:0"
    );
}

#[test]
fn jit_inlined_equality_return_threads_into_caller_condition() {
    // After speculative inlining, a callee's returned equality result reaches the caller's
    // shared `if` condition through an unconditional join. The JIT may thread that edge and
    // branch on equality directly, including the coercing slow path, without materializing a
    // temporary Bool or disturbing other predecessors of the join.
    assert_eq!(
        run_jit(
            "function isOne() { return this.x == 1; }
             function choose(o) { if (o.isOne()) return 7; return 3; }
             var a = { x: 1, isOne: isOne };
             var b = { x: '1', isOne: isOne };
             var c = { x: 2, isOne: isOne };
             var sum = 0;
             for (var i = 0; i < 600; i++) sum += choose(i % 3 === 0 ? a : i % 3 === 1 ? b : c);
             sum"
        ),
        "3400"
    );
}

#[test]
fn jit_seeded_numeric_name_cache_reads_live_mutations() {
    // Second-stage chunks inherit the hot global-name cache and its observed numeric bits. The
    // generated path must compare the live packed property every time: assigning a new value
    // after recompilation falls back to the generic decoder instead of baking a constant.
    assert_eq!(
        run_jit(
            "var HOT_NUMBER = 11;
             function readHot() { return HOT_NUMBER; }
             function outer() { return readHot() + 1; }
             var before;
             for (var i = 0; i < 400; i++) before = outer();
             HOT_NUMBER = 40;
             before + ':' + outer()"
        ),
        "12:41"
    );
}

#[test]
fn jit_cached_name_updates_and_stores_preserve_live_guards() {
    assert_eq!(
        run_jit(
            "function localCase() {
               let x=0, held={id:1}, coercions=0;
               function post(){return x++;}
               function pre(){return ++x;}
               function setX(v){x=v;}
               function current(){return x;}
               function setHeld(v){held=v;return held;}
               for(var i=0;i<600;i++) post();
               var p=post(), q=pre();
               setX({valueOf:function(){coercions++;return 9;}});
               var r=post(), old=held, next={id:2};
               return [p,q,r,current(),coercions,setHeld(next)===next,old.id].join(':');
             }
             globalThis.JIT_NAME_GLOBAL=0;
             function setGlobal(v){JIT_NAME_GLOBAL=v;}
             function incGlobal(){return JIT_NAME_GLOBAL++;}
             for(var i=0;i<600;i++) setGlobal(i);
             var before=JIT_NAME_GLOBAL, gets=0, sets=0, setterSeen=-1;
             Object.defineProperty(globalThis,'JIT_NAME_GLOBAL',{
               configurable:true,
               get:function(){gets++;return 40;},
               set:function(v){sets++;setterSeen=v;}
             });
             setGlobal(9);
             var old=incGlobal();
             var accessor=[before,setterSeen,old,JIT_NAME_GLOBAL,gets,sets].join(':');
             Object.defineProperty(globalThis,'JIT_NAME_GLOBAL',{
               configurable:true,value:7,writable:false
             });
             setGlobal(99);
             localCase()+'|'+accessor+':'+JIT_NAME_GLOBAL"
        ),
        "600:602:9:10:1:true:1|599:41:40:40:2:2:7"
    );
}

#[test]
fn jit_array_push_pop_intrinsics_preserve_live_guards_and_ownership() {
    assert_eq!(
        run_jit(
            "function pushOne(a,v){return a.push(v);}
             function popOne(a){return a.pop();}
             var warm=[];
             for(var i=0;i<700;i++) pushOne(warm,{id:i});
             for(var i=0;i<700;i++) popOne(warm);

             var held={id:41}, a=[];
             var n=pushOne(a,held), same=a[0]===held, out=popOne(a);

             var generic={length:0,push:Array.prototype.push};
             var gn=pushOne(generic,17);

             var setterSeen=-1;
             Object.defineProperty(Array.prototype,'0',{
               configurable:true,set:function(v){setterSeen=v;}
             });
             var guarded=[], sn=pushOne(guarded,23);
             delete Array.prototype[0];

             var locked=[];
             Object.defineProperty(locked,'length',{writable:false});
             var pushThrew=false;
             try{pushOne(locked,1);}catch(e){pushThrew=e instanceof TypeError;}

             var fixed=[];
             Object.defineProperty(fixed,'0',{
               value:9,writable:true,enumerable:true,configurable:false
             });
             fixed.length=1;
             var popThrew=false;
             try{popOne(fixed);}catch(e){popThrew=e instanceof TypeError;}

             var overridden=[];
             overridden.push=function(v){return v+100;};
             var override=pushOne(overridden,5);
             [n,same,out===held,a.length,gn,generic[0],generic.length,
              sn,setterSeen,guarded.length,guarded.hasOwnProperty('0'),
              pushThrew,locked.length,popThrew,fixed.length,override].join(':')"
        ),
        "1:true:true:0:1:17:1:1:23:1:false:true:0:true:1:105"
    );
}

#[test]
fn jit_function_call_intrinsic_preserves_target_and_receiver_guards() {
    assert_eq!(
        run_jit(
            "function target(x){this.sum+=x;return this;}
             function via(f,t,x){return f.call(t,x);}
             function target2(x,y){this.sum+=x*y;return this;}
             function via2(f,t,x,y){return f.call(t,x,y);}
             var box={sum:0}, same=true;
             for(var i=0;i<700;i++) same=same&&(via(target,box,1)===box);
             var pair={sum:0}, pairSame=true;
             for(var i=0;i<700;i++) pairSame=pairSame&&(via2(target2,pair,2,3)===pair);

             function closure(seed){return function(x){this.sum+=seed+x;return this;};}
             var closed=closure(3), cbox={sum:0};
             var cv=via(closed,cbox,4)===cbox;

             function boom(x){throw x;}
             var thrown=-1;
             try{via(boom,box,91);}catch(e){thrown=e;}

             var own=function(x){return x;};
             own.call=function(t,x){return t.base+x+100;};
             var overridden=via(own,{base:5},6);

             var trapCount=0;
             var prox=new Proxy(function(x){return x;},{
               apply:function(t,receiver,args){trapCount++;return receiver.base+args[0];}
             });
             var proxyValue=via(prox,{base:8},9);
             [same,box.sum,pairSame,pair.sum,cv,cbox.sum,thrown,
              overridden,proxyValue,trapCount].join(':')"
        ),
        "true:700:true:4200:true:7:91:111:17:1"
    );
}

#[test]
fn interp_layout_probes() {
    // The asm call thunk's foundation: every probed Interp offset must resolve, and the Vec
    // header word probes must find three distinct words. Fails closed at runtime (valid=false
    // simply disables the thunk), but a probe failure on the dev platform should be loud.
    let mut e = crate::Engine::new();
    e.set_tier(crate::bytecode::Tier::Jit);
    // Force a JIT compile so the layout initializes through the production path.
    let _ = e.eval(
        "function f(a){ return a + 1; } for (var i = 0; i < 64; i++) f(i);",
        false,
    );
    let l = e.interp.interp_layout.get();
    assert!(l.valid, "interp layout probe failed on this platform");
    let mut offs = [
        l.depth,
        l.gc_tick,
        l.gc_next,
        l.cur_coro,
        l.constructing,
        l.new_target,
        l.pending_tail,
        l.fn_frames,
        l.frame_pool,
    ];
    offs.sort_unstable();
    for w in offs.windows(2) {
        assert_ne!(w[0], w[1], "two probed fields share an offset");
    }
    let words = |a: usize, b: usize, c: usize| {
        let mut v = [a, b, c];
        v.sort_unstable();
        v == [0, 8, 16]
    };
    assert!(words(l.fnf_ptr_word, l.fnf_len_word, l.fnf_cap_word));
    assert!(words(l.fp_ptr_word, l.fp_len_word, l.fp_cap_word));
}
