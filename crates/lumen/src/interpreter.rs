//! Tree-walking interpreter: lexical environments, the prototype-based object model, and the
//! ECMAScript abstract operations (ToNumber/ToString/ToBoolean/ToPrimitive, equality, etc.).
//!
//! Control flow uses [`Abrupt`] threaded through `Result`: expressions can only ever raise
//! `Throw`, while statements additionally produce `Return`/`Break`/`Continue` completions.

use crate::ast::*;
use crate::value::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type Env = Rc<RefCell<Scope>>;

pub struct Scope {
    pub vars: HashMap<String, Binding>,
    pub parent: Option<Env>,
    /// For a `with (obj)` block: identifier resolution checks `obj`'s properties before the parent.
    pub with_obj: Option<Value>,
}

pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    /// `false` while a `let`/`const` is in its temporal dead zone.
    pub initialized: bool,
}

pub fn new_scope(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(Scope { vars: HashMap::new(), parent, with_obj: None }))
}

/// A `with (obj)` environment: identifier lookups consult `obj` before the enclosing scope.
pub fn new_with_scope(parent: Env, obj: Value) -> Env {
    Rc::new(RefCell::new(Scope { vars: HashMap::new(), parent: Some(parent), with_obj: Some(obj) }))
}

/// A non-local completion. Expressions only raise `Throw`; the rest flow out of statements.
pub enum Abrupt {
    Throw(Value),
    Return(Value),
    Break(Option<String>),
    Continue(Option<String>),
}

pub type Completion = Result<Value, Abrupt>;

/// How [`Interp::bind_pattern`] should bind the identifiers it reaches.
#[derive(Clone, Copy)]
pub enum BindMode {
    /// `var` — assign to the (already-hoisted) function-scoped binding.
    Var,
    /// `let`/`const` — create a fresh lexical binding (`true` = const).
    Lexical(bool),
}

/// Collect every identifier bound by a pattern (for `var` hoisting and TDZ pre-declaration).
pub fn pattern_idents(pat: &Pattern, out: &mut Vec<String>) {
    match pat {
        Pattern::Ident(n) => out.push(n.clone()),
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, .. } => pattern_idents(pattern, out),
                    ArrayPatElem::Rest(p) => pattern_idents(p, out),
                }
            }
        }
        Pattern::Object(o) => {
            for p in &o.props {
                pattern_idents(&p.value, out);
            }
            if let Some(r) = &o.rest {
                out.push(r.clone());
            }
        }
        Pattern::Member(_) => {}
    }
}

pub struct Interp {
    pub global: Gc,
    pub global_env: Env,
    pub object_proto: Gc,
    pub function_proto: Gc,
    pub array_proto: Gc,
    pub string_proto: Gc,
    pub number_proto: Gc,
    pub boolean_proto: Gc,
    pub symbol_proto: Gc,
    pub error_protos: HashMap<&'static str, Gc>,
    /// Monotonic id source + registry for live symbols (so a symbol used as a property key can be
    /// recovered for `Object.getOwnPropertySymbols`). `sym_for` backs the `Symbol.for` registry.
    pub sym_counter: u64,
    pub sym_registry: HashMap<u64, Rc<SymbolData>>,
    pub sym_for: HashMap<String, Rc<SymbolData>>,
    pub console: Vec<String>,
    /// Current strict-mode flag (pushed/popped around function bodies).
    pub strict: bool,
    /// Live interpreter recursion depth (expression eval + calls). Bounded by [`MAX_EVAL_DEPTH`]
    /// so runaway recursion throws a RangeError instead of overflowing the native stack.
    pub depth: u32,
    /// Per-class metadata (instance fields + whether the class extends another), keyed by the
    /// constructor object's pointer (`Rc::as_ptr(..) as usize`). Lets `construct`/`super` run field
    /// initializers without attaching engine data to the `Object` itself.
    pub class_info: HashMap<usize, ClassInfo>,
    /// The global `eval` function object, so a *direct* eval call (`eval(src)` by that name) can be
    /// distinguished from an indirect one and run in the caller's scope.
    pub eval_fn: Option<Gc>,
    /// `Symbol.iterator`, cached so the iterator protocol can look up `obj[@@iterator]` cheaply.
    pub iterator_sym: Option<Rc<SymbolData>>,
    /// Set while a `?.` link in the current optional chain saw a nullish base, so the rest of the
    /// chain short-circuits to `undefined`. Reset at each `OptionalChain` boundary.
    pub short_circuit: bool,
    /// Backing store for Map/Set/WeakMap/WeakSet instances (ordered entries), keyed by the object's
    /// pointer — the engine analogue of an internal `[[MapData]]` slot.
    pub map_data: HashMap<usize, Vec<(Value, Value)>>,
    /// Prototypes for builtins created after `new()` (Map/Set/Date/...), looked up by name so their
    /// native constructors can stamp the right `[[Prototype]]`.
    pub extra_protos: HashMap<&'static str, Gc>,
    /// ArrayBuffer byte storage, keyed by the ArrayBuffer object's pointer.
    pub array_buffers: HashMap<usize, Vec<u8>>,
    /// TypedArray view state, keyed by the typed-array object's pointer.
    pub typed_arrays: HashMap<usize, TaInfo>,
    /// DataView state `(buffer ptr, byteOffset, byteLength)`, keyed by the DataView's pointer.
    pub data_views: HashMap<usize, (usize, usize, usize)>,
    /// Compiled regular expressions, keyed by the RegExp object's pointer.
    pub regexps: HashMap<usize, Rc<crate::regex::Regex>>,
    /// Proxy `(target, handler)` pairs, keyed by the proxy object's pointer.
    pub proxies: HashMap<usize, (Value, Value)>,
    /// Promise state keyed by the promise object's pointer.
    pub promises: HashMap<usize, PromiseState>,
    /// Temporal object internal slots, keyed by the object's pointer.
    pub temporal: HashMap<usize, crate::temporal::Temporal>,
    /// The microtask queue (drained after the main script by [`crate::Engine::eval`]).
    pub microtasks: std::collections::VecDeque<Job>,
    /// When a generator body is being run eagerly, `yield`ed values are collected here instead of
    /// suspending (lumen has no coroutine support — see `run_generator`).
    pub yield_buffer: Option<Vec<Value>>,
    /// Live-object count above which the next allocation safe point runs the cycle collector.
    pub gc_next: i64,
    /// True while a native constructor is being invoked via `new` (lets e.g. `Number`/`String`
    /// build a wrapper object instead of returning a primitive).
    pub constructing: bool,
}

/// A queued microtask: running one promise reaction.
pub struct Job {
    pub handler: Value,
    pub result: Value,
    pub value: Value,
    pub fulfilled: bool,
}

#[derive(Default)]
pub struct PromiseState {
    /// 0 = pending, 1 = fulfilled, 2 = rejected.
    pub status: u8,
    pub value: Value,
    /// Pending reactions: `(onFulfilled, onRejected, resultPromise)`.
    pub reactions: Vec<(Value, Value, Value)>,
}


/// Engine-side metadata for a class constructor (see [`Interp::class_info`]).
pub struct ClassInfo {
    /// Instance fields: `(property key, optional initializer)`, in declaration order.
    pub fields: Vec<(String, Option<Expr>)>,
    /// The environment field initializers evaluate in (carries the class's super bindings).
    pub field_env: Env,
    /// True if the class has an `extends` clause (derived: `this` is set up by `super()`).
    pub derived: bool,
}

/// Recursion ceiling for the interpreter. Paired with the large worker-thread stacks the runner
/// uses; beyond this we raise "Maximum call stack size exceeded" (a RangeError).
pub const MAX_EVAL_DEPTH: u32 = 1500;

/// Live-object ceiling (≈ a few hundred MB). When a safe point sees this many *live* objects, the
/// cycle collector runs; if it can't get back under, a RangeError is thrown rather than exhausting
/// RAM. This bounds genuine retention; transient cyclic garbage is reclaimed and doesn't count.
pub const MAX_LIVE: i64 = 3_000_000;

/// Live-object count at which the collector first runs; the threshold then floats (see `gc_check`).
pub const GC_TRIGGER: i64 = 200_000;

/// Memory safety valves. lumen has no garbage collector and several built-ins iterate/allocate in
/// proportion to a user-controlled `length`, so without these a single adversarial test (e.g.
/// `Array(4e9).join()` or `s += s` doubling a string) can exhaust all RAM. Operations that would
/// materialize more than these bounds raise a RangeError instead. They are generous relative to
/// real test262 tests but small enough that one runaway test stays bounded.
pub const MAX_ARRAY_OP_LEN: usize = 1 << 20; // ~1M elements
pub const MAX_STR_LEN: usize = 1 << 24; // ~16M bytes

impl Interp {
    pub fn new() -> Interp {
        let object_proto = Object::new(None);
        let function_proto = Object::new(Some(object_proto.clone()));
        let array_proto = Object::new(Some(object_proto.clone()));
        let string_proto = Object::new(Some(object_proto.clone()));
        let number_proto = Object::new(Some(object_proto.clone()));
        let boolean_proto = Object::new(Some(object_proto.clone()));
        // These prototypes are themselves wrapper exotics with default primitive data, so e.g.
        // `Number.prototype.valueOf()` / `Number.prototype == 0` work.
        string_proto.borrow_mut().exotic = Exotic::StrWrap(Rc::from(""));
        number_proto.borrow_mut().exotic = Exotic::NumWrap(0.0);
        boolean_proto.borrow_mut().exotic = Exotic::BoolWrap(false);
        let symbol_proto = Object::new(Some(object_proto.clone()));
        let global = Object::new(Some(object_proto.clone()));
        let global_env = new_scope(None);
        let mut interp = Interp {
            global,
            global_env,
            object_proto,
            function_proto,
            array_proto,
            string_proto,
            number_proto,
            boolean_proto,
            symbol_proto,
            error_protos: HashMap::new(),
            sym_counter: 0,
            sym_registry: HashMap::new(),
            sym_for: HashMap::new(),
            console: Vec::new(),
            strict: false,
            depth: 0,
            class_info: HashMap::new(),
            eval_fn: None,
            iterator_sym: None,
            short_circuit: false,
            map_data: HashMap::new(),
            extra_protos: HashMap::new(),
            array_buffers: HashMap::new(),
            typed_arrays: HashMap::new(),
            data_views: HashMap::new(),
            regexps: HashMap::new(),
            proxies: HashMap::new(),
            promises: HashMap::new(),
            temporal: HashMap::new(),
            microtasks: std::collections::VecDeque::new(),
            yield_buffer: None,
            gc_next: GC_TRIGGER,
            constructing: false,
        };
        crate::builtins::install(&mut interp);
        // `this` at the top level is the global object (sloppy mode).
        let g = Value::Obj(interp.global.clone());
        interp.global_env.borrow_mut().vars.insert(
            "this".to_string(),
            Binding { value: g, mutable: false, initialized: true },
        );
        interp
    }

    // ----- error helpers ----------------------------------------------------------------------

    pub fn make_error(&self, kind: &str, message: impl Into<String>) -> Value {
        let proto = self
            .error_protos
            .get(kind)
            .cloned()
            .unwrap_or_else(|| self.error_protos["Error"].clone());
        let obj = Object::new(Some(proto));
        obj.borrow_mut().exotic = Exotic::Error;
        let msg = message.into();
        if !msg.is_empty() {
            obj.borrow_mut().props.insert("message", Property::builtin(Value::from_string(msg)));
        }
        Value::Obj(obj)
    }
    pub fn throw(&self, kind: &str, message: impl Into<String>) -> Abrupt {
        Abrupt::Throw(self.make_error(kind, message))
    }
    #[allow(dead_code)]
    pub fn type_err<T>(&self, message: impl Into<String>) -> Result<T, Abrupt> {
        Err(self.throw("TypeError", message))
    }

    // ----- typed arrays -----------------------------------------------------------------------

    /// Read element `idx` of a TypedArray as a Number (or undefined if out of range / detached).
    pub fn ta_read(&self, info: &TaInfo, idx: usize) -> Value {
        if idx >= info.len {
            return Value::Undefined;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        match self.array_buffers.get(&info.buffer) {
            Some(buf) if start + es <= buf.len() => {
                let bytes = &buf[start..start + es];
                if info.kind.is_bigint() {
                    Value::BigInt(info.kind.read_bigint(bytes))
                } else {
                    Value::Num(info.kind.read(bytes))
                }
            }
            _ => Value::Undefined,
        }
    }

    /// Store a JS value into a TypedArray element, coercing per the element type. BigInt arrays
    /// require a BigInt (TypeError otherwise); numeric arrays coerce with ToNumber.
    pub fn ta_store(&mut self, info: &TaInfo, idx: usize, v: &Value) -> Result<(), Abrupt> {
        if info.kind.is_bigint() {
            let n = self.to_bigint(v)?;
            self.ta_write_bigint(info, idx, n);
        } else {
            let n = self.to_number(v)?;
            self.ta_write(info, idx, n);
        }
        Ok(())
    }

    /// Write a BigInt (i128) into element `idx` (out-of-range writes are ignored).
    pub fn ta_write_bigint(&mut self, info: &TaInfo, idx: usize, n: i128) {
        if idx >= info.len {
            return;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let bytes = info.kind.write_bigint(n);
        if let Some(buf) = self.array_buffers.get_mut(&info.buffer) {
            if start + es <= buf.len() {
                buf[start..start + es].copy_from_slice(&bytes);
            }
        }
    }

    /// Write Number `n` into element `idx` of a TypedArray (out-of-range writes are ignored).
    pub fn ta_write(&mut self, info: &TaInfo, idx: usize, n: f64) {
        if idx >= info.len {
            return;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let bytes = info.kind.write(n);
        if let Some(buf) = self.array_buffers.get_mut(&info.buffer) {
            if start + es <= buf.len() {
                buf[start..start + es].copy_from_slice(&bytes);
            }
        }
    }

    // ----- symbols ----------------------------------------------------------------------------

    /// Mint a fresh symbol and register it (so it can be recovered from a property key later).
    pub fn new_symbol(&mut self, description: Option<Rc<str>>) -> Value {
        self.sym_counter += 1;
        let data = Rc::new(SymbolData { id: self.sym_counter, description });
        self.sym_registry.insert(data.id, data.clone());
        Value::Sym(data)
    }

    /// The internal property-map key a symbol maps to. A leading NUL never appears in a real
    /// JS-authored property name in the suite, so it cleanly separates symbol keys from string keys.
    pub fn sym_key(data: &SymbolData) -> String {
        format!("\u{0}{}", data.id)
    }
    pub fn is_sym_key(key: &str) -> bool {
        key.starts_with('\u{0}')
    }
    /// Recover the symbol `Value` behind an internal symbol key (for `getOwnPropertySymbols`).
    pub fn sym_from_key(&self, key: &str) -> Option<Value> {
        let id: u64 = key.strip_prefix('\u{0}')?.parse().ok()?;
        self.sym_registry.get(&id).map(|d| Value::Sym(d.clone()))
    }

    // ----- object construction ----------------------------------------------------------------

    pub fn new_object(&self) -> Gc {
        Object::new(Some(self.object_proto.clone()))
    }

    pub fn make_array(&self, items: Vec<Value>) -> Value {
        let obj = Object::new(Some(self.array_proto.clone()));
        obj.borrow_mut().exotic = Exotic::Array;
        let len = items.len();
        {
            let mut b = obj.borrow_mut();
            for (i, v) in items.into_iter().enumerate() {
                b.props.insert(i.to_string(), Property::plain(v));
            }
            b.props.insert("length", Property::data(Value::Num(len as f64), true, false, false));
        }
        Value::Obj(obj)
    }

    /// Build a generator object (an iterator) over an eagerly-collected `buffer`, ending with the
    /// `return` value, or throwing `thrown` once the buffer is exhausted.
    fn make_generator(&mut self, buffer: Vec<Value>, ret: Value, thrown: Option<Value>) -> Value {
        let arr = self.make_array(buffer);
        let proto = self
            .extra_protos
            .get("%IteratorPrototype%")
            .cloned()
            .or_else(|| Some(self.object_proto.clone()));
        let obj = Object::new(proto);
        {
            let mut b = obj.borrow_mut();
            let ro = |v: Value| Property::data(v, true, false, false);
            b.props.insert("__gen_arr", ro(arr));
            b.props.insert("__gen_idx", ro(Value::Num(0.0)));
            b.props.insert("__gen_ret", ro(ret));
            b.props.insert("__gen_haserr", ro(Value::Bool(thrown.is_some())));
            b.props.insert("__gen_err", ro(thrown.unwrap_or(Value::Undefined)));
        }
        self.def_method(&obj, "next", 0, crate::builtins::generator_next);
        Value::Obj(obj)
    }

    pub fn make_native(&self, name: &str, len: usize, f: NativeFn) -> Gc {
        let obj = Object::new(Some(self.function_proto.clone()));
        {
            let mut b = obj.borrow_mut();
            b.call = Callable::Native(f);
            b.props.insert("length", Property::data(Value::Num(len as f64), false, false, true));
            b.props
                .insert("name", Property::data(Value::from_string(name.to_string()), false, false, true));
        }
        obj
    }

    /// Define a native method on `target` (non-enumerable, as built-ins are).
    pub fn def_method(&self, target: &Gc, name: &str, len: usize, f: NativeFn) {
        let func = self.make_native(name, len, f);
        target.borrow_mut().props.insert(name, Property::builtin(Value::Obj(func)));
    }

    pub fn make_function(&self, func: Rc<Function>, env: Env) -> Value {
        let obj = Object::new(Some(self.function_proto.clone()));
        let arity = func.params.iter().take_while(|p| p.default.is_none() && !p.rest).count();
        let name = func.name.clone().unwrap_or_default();
        let is_arrow = func.is_arrow;
        {
            let mut b = obj.borrow_mut();
            b.call = Callable::User(func, env);
            b.props.insert("length", Property::data(Value::Num(arity as f64), false, false, true));
            b.props
                .insert("name", Property::data(Value::from_string(name), false, false, true));
        }
        if !is_arrow {
            // Non-arrow functions get a fresh `prototype` object with a back-reference.
            let proto = self.new_object();
            proto
                .borrow_mut()
                .props
                .insert("constructor", Property::builtin(Value::Obj(obj.clone())));
            obj.borrow_mut()
                .props
                .insert("prototype", Property::data(Value::Obj(proto), true, false, false));
            obj.borrow_mut().is_constructor = true;
        }
        Value::Obj(obj)
    }

    // ----- property access --------------------------------------------------------------------

    /// Get `base[key]`, walking the prototype chain and invoking getters. Primitive bases are
    /// handled by routing to their wrapper prototype (and string index/length specially).
    pub fn get_member(&mut self, base: &Value, key: &str) -> Result<Value, Abrupt> {
        match base {
            Value::Undefined | Value::Null => {
                Err(self.throw("TypeError", format!("cannot read property '{key}' of {}", type_name(base))))
            }
            Value::Str(s) => {
                if key == "length" {
                    return Ok(Value::Num(s.chars().count() as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(match s.chars().nth(i) {
                        Some(c) => Value::from_string(c.to_string()),
                        None => Value::Undefined,
                    });
                }
                let proto = self.string_proto.clone();
                self.get_from_chain(&proto, key, base)
            }
            Value::Num(_) => {
                let proto = self.number_proto.clone();
                self.get_from_chain(&proto, key, base)
            }
            Value::Bool(_) => {
                let proto = self.boolean_proto.clone();
                self.get_from_chain(&proto, key, base)
            }
            Value::Sym(s) => {
                if key == "description" {
                    return Ok(s.description.clone().map(Value::Str).unwrap_or(Value::Undefined));
                }
                let proto = self.symbol_proto.clone();
                self.get_from_chain(&proto, key, base)
            }
            Value::BigInt(_) => match self.extra_protos.get("BigInt").cloned() {
                Some(proto) => self.get_from_chain(&proto, key, base),
                None => Ok(Value::Undefined),
            },
            Value::Obj(o) => {
                let o = o.clone();
                let ptr = Rc::as_ptr(&o) as usize;
                // String wrapper (`new String(...)`/`Object("...")`): own indexed chars + `length`.
                if let Exotic::StrWrap(s) = o.borrow().exotic.clone() {
                    if key == "length" {
                        return Ok(Value::Num(s.chars().count() as f64));
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        if let Some(c) = s.chars().nth(i) {
                            return Ok(Value::from_string(c.to_string()));
                        }
                    }
                }
                // Proxy: invoke the `get` trap, or forward to the target.
                if !self.proxies.is_empty() {
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        let trap = self.get_member(&handler, "get")?;
                        if trap.is_callable() {
                            return self.call(trap, handler, &[target, Value::str(key), base.clone()]);
                        }
                        return self.get_member(&target, key);
                    }
                }
                // TypedArray integer-index reads come from the backing buffer, not the property map.
                if let Some(info) = self.typed_arrays.get(&ptr).copied() {
                    if let Ok(idx) = key.parse::<usize>() {
                        return Ok(self.ta_read(&info, idx));
                    }
                }
                self.get_from_chain(&o, key, base)
            }
        }
    }

    fn get_from_chain(&mut self, start: &Gc, key: &str, receiver: &Value) -> Result<Value, Abrupt> {
        let mut cur = Some(start.clone());
        while let Some(obj) = cur {
            let prop = obj.borrow().props.get(key).cloned();
            if let Some(p) = prop {
                if p.accessor {
                    return match p.get {
                        Some(getter) => self.call(getter, receiver.clone(), &[]),
                        None => Ok(Value::Undefined),
                    };
                }
                return Ok(p.value);
            }
            cur = obj.borrow().proto.clone();
        }
        Ok(Value::Undefined)
    }

    /// Set `base[key] = value`, honouring setters, accessor-only properties, read-only data
    /// properties, and array `length`/index bookkeeping.
    pub fn set_member(&mut self, base: &Value, key: &str, value: Value) -> Result<(), Abrupt> {
        let obj = match base {
            Value::Obj(o) => o.clone(),
            Value::Undefined | Value::Null => {
                return Err(self.throw("TypeError", format!("cannot set property '{key}' of {}", type_name(base))))
            }
            // Setting a property on a primitive is a no-op in sloppy mode (and TypeError in strict,
            // which we approximate as a no-op for now).
            _ => return Ok(()),
        };

        let ptr = Rc::as_ptr(&obj) as usize;
        // Proxy: invoke the `set` trap, or forward to the target.
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                let trap = self.get_member(&handler, "set")?;
                if trap.is_callable() {
                    self.call(trap, handler, &[target, Value::str(key), value, base.clone()])?;
                    return Ok(());
                }
                return self.set_member(&target, key, value);
            }
        }
        // TypedArray integer-index writes go straight to the backing buffer.
        if let Some(info) = self.typed_arrays.get(&ptr).copied() {
            if let Ok(idx) = key.parse::<usize>() {
                self.ta_store(&info, idx, &value)?;
                return Ok(());
            }
        }

        // Walk the chain for an accessor or read-only data property.
        let mut cur = Some(obj.clone());
        while let Some(o) = cur {
            let prop = o.borrow().props.get(key).cloned();
            if let Some(p) = prop {
                if p.accessor {
                    return match p.set {
                        Some(setter) => {
                            self.call(setter, base.clone(), &[value])?;
                            Ok(())
                        }
                        None => {
                            if self.strict {
                                Err(self.throw("TypeError", format!("cannot set getter-only property '{key}'")))
                            } else {
                                Ok(())
                            }
                        }
                    };
                }
                if Rc::ptr_eq(&o, &obj) {
                    if !p.writable {
                        if self.strict {
                            return Err(self.throw(
                                "TypeError",
                                format!("cannot assign to read-only property '{key}'"),
                            ));
                        }
                        return Ok(());
                    }
                    break; // own writable data property — update below
                }
                if !p.writable {
                    if self.strict {
                        return Err(self.throw("TypeError", format!("cannot assign to read-only property '{key}'")));
                    }
                    return Ok(());
                }
                break; // inherited writable data property — create own on receiver
            }
            cur = o.borrow().proto.clone();
        }

        let is_array = matches!(obj.borrow().exotic, Exotic::Array);
        if is_array {
            self.array_set(&obj, key, value)?;
        } else {
            let existed = obj.borrow().props.contains(key);
            if existed {
                if let Some(p) = obj.borrow_mut().props.get_mut(key) {
                    p.value = value;
                }
            } else {
                if !obj.borrow().extensible {
                    if self.strict {
                        return Err(self.throw("TypeError", "cannot add property, object is not extensible"));
                    }
                    return Ok(());
                }
                obj.borrow_mut().props.insert(key, Property::plain(value));
            }
        }
        Ok(())
    }

    fn array_set(&mut self, obj: &Gc, key: &str, value: Value) -> Result<(), Abrupt> {
        if key == "length" {
            let n = self.to_number(&value)?;
            // Array lengths are uint32 (ToUint32 round-trips exactly, else "Invalid array length").
            if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > 4294967295.0 {
                return Err(self.throw("RangeError", "Invalid array length"));
            }
            // A non-writable `length` (frozen/sealed array) rejects the change.
            let len_writable =
                obj.borrow().props.get("length").map(|p| p.writable).unwrap_or(true);
            if !len_writable {
                if self.strict {
                    return Err(self.throw("TypeError", "cannot assign to read-only property 'length'"));
                }
                return Ok(());
            }
            let new_len = n as usize;
            let old_len = self.array_length(obj);
            if new_len < old_len {
                // Remove only the index properties that actually exist and are now out of range —
                // never loop over the (possibly huge) numeric range new_len..old_len.
                let to_remove: Vec<String> = obj
                    .borrow()
                    .props
                    .keys()
                    .into_iter()
                    .filter(|k| k.parse::<usize>().map(|i| i >= new_len).unwrap_or(false))
                    .map(|k| k.to_string())
                    .collect();
                for k in to_remove {
                    obj.borrow_mut().props.remove(&k);
                }
            }
            obj.borrow_mut()
                .props
                .insert("length", Property::data(Value::Num(new_len as f64), true, false, false));
            return Ok(());
        }
        // Adding a new index to a non-extensible (sealed/frozen) array is rejected.
        if !obj.borrow().props.contains(key) && !obj.borrow().extensible {
            if self.strict {
                return Err(self.throw("TypeError", "cannot add property, object is not extensible"));
            }
            return Ok(());
        }
        obj.borrow_mut().props.insert(key, Property::plain(value));
        // Only a canonical array index (< 2^32 - 1) updates `length`; larger numeric keys are
        // ordinary properties.
        if let Some(i) = key.parse::<u64>().ok().filter(|&i| i < 4294967295) {
            let i = i as usize;
            let len = self.array_length(obj);
            if i >= len {
                obj.borrow_mut().props.insert(
                    "length",
                    Property::data(Value::Num((i + 1) as f64), true, false, false),
                );
            }
        }
        Ok(())
    }

    pub fn array_length(&self, obj: &Gc) -> usize {
        match obj.borrow().props.get("length").map(|p| p.value.clone()) {
            Some(Value::Num(n)) => n as usize,
            _ => 0,
        }
    }

    /// Array length for an operation that will iterate/allocate proportional to it. Errors with a
    /// RangeError past [`MAX_ARRAY_OP_LEN`] so a huge `.length` cannot exhaust memory.
    pub fn checked_array_len(&mut self, obj: &Gc) -> Result<usize, Abrupt> {
        let len = self.to_length(obj)?;
        if len > MAX_ARRAY_OP_LEN {
            return Err(self.throw("RangeError", "array length exceeds engine limit"));
        }
        Ok(len)
    }

    /// ToLength of an array-like's `length` property (coercing string/object lengths), clamped to
    /// the 2^53-1 spec maximum.
    pub fn to_length(&mut self, obj: &Gc) -> Result<usize, Abrupt> {
        let v = self.get_member(&Value::Obj(obj.clone()), "length")?;
        let n = self.to_number(&v)?;
        Ok(if n.is_nan() || n <= 0.0 {
            0
        } else {
            n.trunc().min(9007199254740991.0) as usize
        })
    }

    // ----- garbage collection -----------------------------------------------------------------

    /// Allocation safe point. When live objects pass the floating threshold, run the cycle
    /// collector; if genuine retention still exceeds `MAX_LIVE`, throw rather than exhaust RAM.
    pub(crate) fn gc_check(&mut self) -> Result<(), Abrupt> {
        if crate::value::live_objects() <= self.gc_next {
            return Ok(());
        }
        self.gc_collect();
        let live = crate::value::live_objects();
        if live > MAX_LIVE {
            return Err(self.throw("RangeError", "allocation limit exceeded"));
        }
        // Re-arm: collect again once live doubles, clamped to [GC_TRIGGER, MAX_LIVE].
        self.gc_next = (live.saturating_mul(2)).clamp(GC_TRIGGER, MAX_LIVE);
        Ok(())
    }

    /// The object references *to other heap objects* held directly by `o` (proto, property
    /// values/getters/setters, and bound-function target/this/args). Collected into a Vec so `o`'s
    /// borrow is released before callers re-borrow — important for self-referential objects.
    fn obj_refs(o: &Gc) -> Vec<Gc> {
        let b = o.borrow();
        let mut refs = Vec::new();
        if let Some(p) = &b.proto {
            refs.push(p.clone());
        }
        for (_, prop) in b.props.iter() {
            if let Value::Obj(p) = &prop.value {
                refs.push(p.clone());
            }
            if let Some(Value::Obj(p)) = &prop.get {
                refs.push(p.clone());
            }
            if let Some(Value::Obj(p)) = &prop.set {
                refs.push(p.clone());
            }
        }
        if let Callable::Bound { target, this, args } = &b.call {
            refs.push(target.clone());
            if let Value::Obj(p) = this {
                refs.push(p.clone());
            }
            for a in args {
                if let Value::Obj(p) = a {
                    refs.push(p.clone());
                }
            }
        }
        refs
    }

    /// Refcount-based cycle collector. An object whose `Rc::strong_count` exceeds the references it
    /// receives from other heap objects has an *external* holder — the Rust stack, a scope, the
    /// global, or a side table — so it (and everything it reaches) is live. Everything else is
    /// referenced only from within unreachable cycles and is reclaimed by breaking its references.
    /// This needs no root enumeration, so it is safe to run in the middle of evaluation.
    pub(crate) fn gc_collect(&mut self) {
        let live = crate::value::gc_snapshot();

        // Reset scratch, then count references between heap objects.
        for o in &live {
            let b = o.borrow();
            b.gc_mark.set(false);
            b.gc_internal.set(0);
        }
        for o in &live {
            for p in Self::obj_refs(o) {
                let pb = p.borrow();
                pb.gc_internal.set(pb.gc_internal.get() + 1);
            }
        }

        // Roots: objects with a reference from outside the heap-object graph. `strong_count` here
        // includes exactly one clone held by `live`, so external refs == strong - internal - 1.
        let mut stack: Vec<Gc> = Vec::new();
        for o in &live {
            let internal = o.borrow().gc_internal.get() as usize;
            if Rc::strong_count(o) > internal + 1 {
                o.borrow().gc_mark.set(true);
                stack.push(o.clone());
            }
        }
        // Mark everything reachable from the roots.
        while let Some(o) = stack.pop() {
            for p in Self::obj_refs(&o) {
                if !p.borrow().gc_mark.get() {
                    p.borrow().gc_mark.set(true);
                    stack.push(p);
                }
            }
        }

        // Sweep: clear unmarked (garbage) objects to break their cycles; once `live` drops, their
        // refcounts hit zero and they are freed. Also evict them from pointer-keyed side tables so a
        // future object reusing the address can't inherit stale metadata.
        for o in &live {
            if !o.borrow().gc_mark.get() {
                let ptr = Rc::as_ptr(o) as usize;
                self.class_info.remove(&ptr);
                self.map_data.remove(&ptr);
                self.typed_arrays.remove(&ptr);
                self.data_views.remove(&ptr);
                self.regexps.remove(&ptr);
                self.proxies.remove(&ptr);
                self.promises.remove(&ptr);
                self.temporal.remove(&ptr);
                self.array_buffers.remove(&ptr);
                let mut b = o.borrow_mut();
                b.props.clear();
                b.proto = None;
                b.call = Callable::None;
                b.exotic = Exotic::None;
            }
        }
    }

    // ----- calling ----------------------------------------------------------------------------

    pub fn call(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, Abrupt> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        if let Err(e) = self.gc_check() {
            self.depth -= 1;
            return Err(e);
        }
        let r = self.call_inner(callee, this, args);
        self.depth -= 1;
        r
    }

    fn call_inner(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, Abrupt> {
        let obj = match &callee {
            Value::Obj(o) => o.clone(),
            _ => return Err(self.throw("TypeError", format!("{} is not a function", type_name(&callee)))),
        };
        // Proxy with an `apply` trap (or forward to the target).
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&(Rc::as_ptr(&obj) as usize)).cloned() {
                let trap = self.get_member(&handler, "apply")?;
                let arr = self.make_array(args.to_vec());
                if trap.is_callable() {
                    return self.call(trap, handler, &[target, this, arr]);
                }
                return self.call(target, this, args);
            }
        }
        let call = obj.borrow().call.clone();
        // A plain call is never constructing (only `new` sets the flag). Clearing it here keeps a
        // wrapper constructor invoked as a function — `Number(x)` — from boxing.
        let saved_ctor = self.constructing;
        self.constructing = false;
        let r = match call {
            Callable::None => Err(self.throw("TypeError", "value is not a function")),
            Callable::Native(f) => f(self, this, args).map_err(Abrupt::Throw),
            Callable::User(func, env) => self.call_user(&func, env, this, args, false, &obj),
            Callable::Bound { target, this: bthis, args: bargs } => {
                let mut all = bargs.clone();
                all.extend_from_slice(args);
                self.call(Value::Obj(target), bthis, &all)
            }
        };
        self.constructing = saved_ctor;
        r
    }

    pub(crate) fn call_user(
        &mut self,
        func: &Rc<Function>,
        closure: Env,
        this: Value,
        args: &[Value],
        is_construct: bool,
        fn_obj: &Gc,
    ) -> Result<Value, Abrupt> {
        let scope = new_scope(Some(closure));

        if !func.is_arrow {
            // `this` binding. Strict: pass through. Sloppy: undefined/null → global; primitive → box.
            let this_val = if func.is_strict || is_construct {
                this
            } else {
                match this {
                    Value::Undefined | Value::Null => Value::Obj(self.global.clone()),
                    other => other,
                }
            };
            scope.borrow_mut().vars.insert(
                "this".to_string(),
                Binding { value: this_val, mutable: false, initialized: true },
            );
            // A minimal `arguments` array (not the live mapped object).
            let args_arr = self.make_array(args.to_vec());
            scope.borrow_mut().vars.insert(
                "arguments".to_string(),
                Binding { value: args_arr, mutable: true, initialized: true },
            );
            // Expose the callee for named function expressions / recursion via `name`.
            if let Some(name) = &func.name {
                if !scope.borrow().vars.contains_key(name) {
                    scope.borrow_mut().vars.insert(
                        name.clone(),
                        Binding { value: Value::Obj(fn_obj.clone()), mutable: false, initialized: true },
                    );
                }
            }
        }

        self.bind_params(&func.params, args, &scope)?;

        let saved_strict = self.strict;
        self.strict = func.is_strict;

        // Generators run eagerly into a yield buffer (lumen has no coroutines); see run_generator.
        if func.is_generator {
            let gen = self.run_generator(func, &scope);
            self.strict = saved_strict;
            return gen;
        }

        // Hoist `var`/function declarations into the function scope before executing the body.
        self.hoist(&func.body, &scope, true);
        // Pre-declare body-level `let`/`const` in their temporal dead zone.
        self.declare_block_lexicals(&func.body, &scope, false);

        // Async functions: run synchronously (await unwraps settled promises), then wrap the result
        // in a fulfilled/rejected promise.
        let mut result = Ok(Value::Undefined);
        for stmt in &func.body {
            match self.exec_stmt(stmt, &scope) {
                Ok(_) => {}
                Err(Abrupt::Return(v)) => {
                    result = Ok(v);
                    break;
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.strict = saved_strict;
        if func.is_async {
            let promise = self.new_promise();
            match result {
                Ok(v) => self.resolve_promise(&promise, v),
                Err(Abrupt::Throw(e)) => self.reject_promise(&promise, e),
                Err(_) => {}
            }
            return Ok(promise);
        }
        result
    }

    /// Run a generator body eagerly, collecting all `yield`ed values, and return a generator object
    /// (an iterator) over them ending with the `return` value. This is an approximation — side
    /// effects happen up-front and `next(v)` arguments are ignored — but covers many simple cases.
    fn run_generator(&mut self, func: &Rc<Function>, scope: &Env) -> Result<Value, Abrupt> {
        let saved = self.yield_buffer.take();
        self.yield_buffer = Some(Vec::new());
        self.hoist(&func.body, scope, true);
        let mut ret = Value::Undefined;
        let mut thrown: Option<Value> = None;
        for stmt in &func.body {
            match self.exec_stmt(stmt, scope) {
                Ok(_) => {}
                Err(Abrupt::Return(v)) => {
                    ret = v;
                    break;
                }
                Err(Abrupt::Throw(e)) => {
                    thrown = Some(e);
                    break;
                }
                Err(_) => break,
            }
        }
        let buffer = self.yield_buffer.take().unwrap_or_default();
        self.yield_buffer = saved;
        Ok(self.make_generator(buffer, ret, thrown))
    }

    fn bind_params(&mut self, params: &[Param], args: &[Value], scope: &Env) -> Result<(), Abrupt> {
        for (i, p) in params.iter().enumerate() {
            let value = if p.rest {
                let rest: Vec<Value> = args.iter().skip(i).cloned().collect();
                self.make_array(rest)
            } else {
                let mut v = args.get(i).cloned().unwrap_or(Value::Undefined);
                if matches!(v, Value::Undefined) {
                    if let Some(d) = &p.default {
                        v = self.eval(d, scope)?;
                        if let (crate::ast::Pattern::Ident(n), true) =
                            (&p.pattern, crate::eval::is_anonymous_fn(d))
                        {
                            self.set_fn_name(&v, n);
                        }
                    }
                }
                v
            };
            self.bind_pattern(&p.pattern, value, scope, BindMode::Lexical(false))?;
            if p.rest {
                break;
            }
        }
        Ok(())
    }

    pub fn construct(&mut self, callee: Value, args: &[Value]) -> Result<Value, Abrupt> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        let r = self.construct_inner(callee, args);
        self.depth -= 1;
        r
    }

    fn construct_inner(&mut self, callee: Value, args: &[Value]) -> Result<Value, Abrupt> {
        let obj = match &callee {
            Value::Obj(o) => o.clone(),
            _ => return Err(self.throw("TypeError", "value is not a constructor")),
        };
        // Proxy with a `construct` trap (or forward to the target).
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&(Rc::as_ptr(&obj) as usize)).cloned() {
                let trap = self.get_member(&handler, "construct")?;
                let arr = self.make_array(args.to_vec());
                if trap.is_callable() {
                    return self.call(trap, handler, &[target, arr, callee.clone()]);
                }
                return self.construct(target, args);
            }
        }
        let call = obj.borrow().call.clone();
        match call {
            Callable::Native(f) => {
                // A native non-constructor (a method, global function, Math fn) has no own
                // `prototype` property; only real built-in constructors do. Reject `new` on the rest.
                let constructable =
                    obj.borrow().is_constructor || obj.borrow().props.contains("prototype");
                if !constructable {
                    return Err(self.throw("TypeError", "function is not a constructor"));
                }
                // Built-in constructors build and return their own object. The `constructing` flag
                // lets wrapper constructors (Number/String/...) distinguish `new X()` from `X()`.
                let saved = self.constructing;
                self.constructing = true;
                let r = f(self, Value::Undefined, args).map_err(Abrupt::Throw);
                self.constructing = saved;
                r
            }
            Callable::User(func, env) => {
                if func.is_arrow {
                    return Err(self.throw("TypeError", "arrow functions are not constructors"));
                }
                let proto = match obj.borrow().props.get("prototype").map(|p| p.value.clone()) {
                    Some(Value::Obj(p)) => Some(p),
                    _ => Some(self.object_proto.clone()),
                };
                let this = Object::new(proto);
                let this_val = Value::Obj(this);
                // Class constructors run field initializers (and, when derived, defer `this` setup
                // to `super()`); plain function constructors just run their body.
                if self.class_info.contains_key(&(Rc::as_ptr(&obj) as usize)) {
                    self.run_constructor_on(&callee, &this_val, args)?;
                    Ok(this_val)
                } else {
                    let ret = self.call_user(&func, env, this_val.clone(), args, true, &obj)?;
                    Ok(match ret {
                        Value::Obj(_) => ret,
                        _ => this_val,
                    })
                }
            }
            Callable::Bound { target, args: bargs, .. } => {
                let mut all = bargs.clone();
                all.extend_from_slice(args);
                self.construct(Value::Obj(target), &all)
            }
            Callable::None => Err(self.throw("TypeError", "value is not a constructor")),
        }
    }

    // ----- program / statement execution ------------------------------------------------------

    pub fn run_program(&mut self, body: &[Stmt]) -> Result<Value, Value> {
        self.hoist(body, &self.global_env.clone(), true);
        // Top-level `let`/`const` are pre-declared in their temporal dead zone.
        self.declare_block_lexicals(body, &self.global_env.clone(), false);
        let env = self.global_env.clone();
        let mut last = Value::Undefined;
        for stmt in body {
            match self.exec_stmt(stmt, &env) {
                Ok(v) => {
                    if !matches!(v, Value::Undefined) {
                        last = v;
                    }
                }
                Err(Abrupt::Throw(v)) => return Err(v),
                Err(_) => return Ok(last), // stray break/continue/return at top level: stop
            }
        }
        Ok(last)
    }

    /// Hoist `var` and function declarations into `scope`. `let`/`const` get TDZ bindings created
    /// at block entry instead (see [`Self::exec_block`]).
    pub(crate) fn hoist(&mut self, stmts: &[Stmt], scope: &Env, _fn_level: bool) {
        for stmt in stmts {
            self.hoist_stmt(stmt, scope);
        }
        // Function declarations are also initialised eagerly (in source order, after var names).
        for stmt in stmts {
            if let Stmt::FuncDecl(func) = stmt {
                if let Some(name) = &func.name {
                    let f = self.make_function(func.clone(), scope.clone());
                    scope
                        .borrow_mut()
                        .vars
                        .insert(name.clone(), Binding { value: f, mutable: true, initialized: true });
                }
            }
        }
        // Annex B.3.3: in sloppy mode, a function declared inside a block is also bound in the
        // enclosing function/global scope.
        if !self.strict {
            for stmt in stmts {
                self.hoist_block_funcs(stmt, scope, false);
            }
        }
    }

    fn hoist_block_funcs(&mut self, stmt: &Stmt, scope: &Env, in_block: bool) {
        match stmt {
            Stmt::FuncDecl(func) if in_block => {
                if let Some(name) = &func.name {
                    let f = self.make_function(func.clone(), scope.clone());
                    scope.borrow_mut().vars.insert(
                        name.clone(),
                        Binding { value: f, mutable: true, initialized: true },
                    );
                }
            }
            Stmt::Block(body) => {
                for s in body {
                    self.hoist_block_funcs(s, scope, true);
                }
            }
            Stmt::If { cons, alt, .. } => {
                self.hoist_block_funcs(cons, scope, true);
                if let Some(a) = alt {
                    self.hoist_block_funcs(a, scope, true);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::ForInOf { body, .. }
            | Stmt::Labeled { body, .. }
            | Stmt::With { body, .. } => self.hoist_block_funcs(body, scope, true),
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    for s in &c.body {
                        self.hoist_block_funcs(s, scope, true);
                    }
                }
            }
            Stmt::Try { block, handler, finalizer } => {
                for s in block {
                    self.hoist_block_funcs(s, scope, true);
                }
                if let Some((_, h)) = handler {
                    for s in h {
                        self.hoist_block_funcs(s, scope, true);
                    }
                }
                if let Some(f) = finalizer {
                    for s in f {
                        self.hoist_block_funcs(s, scope, true);
                    }
                }
            }
            _ => {}
        }
    }

    fn hoist_stmt(&mut self, stmt: &Stmt, scope: &Env) {
        match stmt {
            Stmt::VarDecl { kind: DeclKind::Var, decls } => {
                for (pat, _) in decls {
                    let mut names = Vec::new();
                    pattern_idents(pat, &mut names);
                    for name in names {
                        if !scope.borrow().vars.contains_key(&name) {
                            scope.borrow_mut().vars.insert(
                                name,
                                Binding { value: Value::Undefined, mutable: true, initialized: true },
                            );
                        }
                    }
                }
            }
            Stmt::If { cons, alt, .. } => {
                self.hoist_stmt(cons, scope);
                if let Some(a) = alt {
                    self.hoist_stmt(a, scope);
                }
            }
            Stmt::Block(body) => {
                for s in body {
                    self.hoist_var_only(s, scope);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::Labeled { body, .. }
            | Stmt::With { body, .. } => self.hoist_stmt(body, scope),
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    if let ForInit::VarDecl { kind: DeclKind::Var, decls } = init.as_ref() {
                        for (pat, _) in decls {
                            let mut names = Vec::new();
                            pattern_idents(pat, &mut names);
                            for name in names {
                                scope.borrow_mut().vars.insert(
                                    name,
                                    Binding { value: Value::Undefined, mutable: true, initialized: true },
                                );
                            }
                        }
                    }
                }
                self.hoist_stmt(body, scope);
            }
            Stmt::ForInOf { decl: Some(DeclKind::Var), left, body, .. } => {
                let mut names = Vec::new();
                pattern_idents(left, &mut names);
                for name in names {
                    scope.borrow_mut().vars.insert(
                        name,
                        Binding { value: Value::Undefined, mutable: true, initialized: true },
                    );
                }
                self.hoist_stmt(body, scope);
            }
            Stmt::ForInOf { body, .. } => self.hoist_stmt(body, scope),
            Stmt::Try { block, handler, finalizer } => {
                for s in block {
                    self.hoist_var_only(s, scope);
                }
                if let Some((_, h)) = handler {
                    for s in h {
                        self.hoist_var_only(s, scope);
                    }
                }
                if let Some(f) = finalizer {
                    for s in f {
                        self.hoist_var_only(s, scope);
                    }
                }
            }
            _ => {}
        }
    }

    /// Like [`Self::hoist_stmt`] but only descends collecting `var`s (used inside nested blocks so
    /// their function-scoped `var`s reach the function scope).
    fn hoist_var_only(&mut self, stmt: &Stmt, scope: &Env) {
        self.hoist_stmt(stmt, scope);
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Undefined => "undefined",
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Num(_) => "number",
        Value::BigInt(_) => "bigint",
        Value::Str(_) => "string",
        Value::Sym(_) => "symbol",
        Value::Obj(_) => "object",
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}
