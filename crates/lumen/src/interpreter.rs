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
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// `$262.agent` wiring. The main agent holds a broadcast sender per spawned agent plus the report
/// receiver; a spawned agent holds its broadcast receiver and a clone of the report sender.
pub struct AgentChannels {
    pub agent_broadcast_txs: Vec<std::sync::mpsc::Sender<(u64, usize)>>,
    pub report_rx: Option<std::sync::mpsc::Receiver<String>>,
    pub report_tx: std::sync::mpsc::Sender<String>,
    pub broadcast_rx: Option<std::sync::mpsc::Receiver<(u64, usize)>>,
}

/// Process-global backing store for SharedArrayBuffer memory, keyed by a unique id so it can be
/// shared across agent threads (each agent runs its own single-threaded `Interp`).
pub type SharedMem = Arc<Mutex<Vec<u8>>>;
pub fn shared_mem_registry() -> &'static Mutex<HashMap<u64, SharedMem>> {
    static R: OnceLock<Mutex<HashMap<u64, SharedMem>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn next_shared_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, AtomicOrdering::SeqCst)
}
/// Allocate a fresh shared-memory block of `len` zero bytes; returns its global id.
pub fn alloc_shared_mem(len: usize) -> u64 {
    let id = next_shared_id();
    shared_mem_registry()
        .lock()
        .unwrap()
        .insert(id, Arc::new(Mutex::new(vec![0u8; len])));
    id
}
pub fn shared_mem_get(id: u64) -> Option<SharedMem> {
    shared_mem_registry().lock().unwrap().get(&id).cloned()
}
/// Milliseconds since a fixed process-global instant (for `$262.agent.monotonicNow`).
pub fn monotonic_now_ms() -> f64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f64() * 1000.0
}

/// A futex-like wait table for `Atomics.wait`/`notify`, keyed by `(shared-memory id, byte index)`.
/// Each blocked waiter registers an individual wake handle so `notify` can wake an exact count.
pub type Waiter = Arc<(Mutex<bool>, Condvar)>;
fn wait_table() -> &'static Mutex<HashMap<(u64, usize), Vec<Waiter>>> {
    static T: OnceLock<Mutex<HashMap<(u64, usize), Vec<Waiter>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Add a waiter to the list for `(id, index)` without blocking. `Atomics.waitAsync` registers
/// synchronously (so a notify later in the same job sees it) and blocks on a helper thread.
pub fn futex_register(id: u64, index: usize) -> Waiter {
    let waiter: Waiter = Arc::new((Mutex::new(false), Condvar::new()));
    wait_table()
        .lock()
        .unwrap()
        .entry((id, index))
        .or_default()
        .push(waiter.clone());
    waiter
}
/// Block on `(id, index)` until notified or `timeout` elapses. Returns `true` if notified (woken),
/// `false` if it timed out.
pub fn futex_wait(id: u64, index: usize, timeout: Option<std::time::Duration>) -> bool {
    let waiter = futex_register(id, index);
    futex_block(&waiter, id, index, timeout)
}
/// Block on an already-registered waiter.
pub fn futex_block(
    waiter: &Waiter,
    id: u64,
    index: usize,
    timeout: Option<std::time::Duration>,
) -> bool {
    let (lock, cvar) = &**waiter;
    let mut woken = lock.lock().unwrap();
    let result = match timeout {
        Some(dur) => {
            let deadline = std::time::Instant::now() + dur;
            loop {
                if *woken {
                    break true;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break false;
                }
                let (g, res) = cvar.wait_timeout(woken, deadline - now).unwrap();
                woken = g;
                if *woken {
                    break true;
                }
                if res.timed_out() {
                    break false;
                }
            }
        }
        None => loop {
            if *woken {
                break true;
            }
            woken = cvar.wait(woken).unwrap();
        },
    };
    // On timeout, remove ourselves from the table (a notify may have already pulled us out).
    if !result {
        if let Some(v) = wait_table().lock().unwrap().get_mut(&(id, index)) {
            v.retain(|w| !Arc::ptr_eq(w, waiter));
        }
    }
    result
}
/// Wake up to `max` (`<0` ⇒ all) waiters on `(id, index)`, returning how many were woken.
pub fn futex_notify(id: u64, index: usize, max: i64) -> u64 {
    let to_wake: Vec<Waiter> = {
        let mut t = wait_table().lock().unwrap();
        if let Some(v) = t.get_mut(&(id, index)) {
            let n = if max < 0 {
                v.len()
            } else {
                (max as usize).min(v.len())
            };
            v.drain(0..n).collect()
        } else {
            Vec::new()
        }
    };
    let count = to_wake.len() as u64;
    for w in to_wake {
        let (lock, cvar) = &*w;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    count
}

pub type Env = Rc<RefCell<Scope>>;

pub struct Scope {
    pub vars: HashMap<String, Binding>,
    pub parent: Option<Env>,
    /// For a `with (obj)` block: identifier resolution checks `obj`'s properties before the parent.
    pub with_obj: Option<Value>,
    /// `true` if this is a *variable* environment (a function body scope, the global scope, or an
    /// `eval` scope) — the target for `var`/function hoisting. Block, `with`, and function *parameter*
    /// scopes are `false`, so a sloppy direct `eval` hoists its vars past them into the nearest
    /// enclosing variable environment (see EvalDeclarationInstantiation).
    pub var_boundary: bool,
    /// `true` for a `catch` clause's parameter environment. A sloppy direct `eval`'s
    /// EvalDeclarationInstantiation walk skips it, so `eval("var e")` inside `catch (e) { … }` is
    /// allowed (the web-compatibility carve-out for VariableStatements in catch blocks).
    pub catch_param: bool,
    /// Names declared *lexically* at this scope's own level (let/const/using/class). A function
    /// body scope holds both hoisted vars and body-level lexicals; a sloppy direct eval's
    /// var/lexical conflict check needs to tell them apart.
    pub lexical_names: Vec<String>,
}

#[derive(Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
    /// `false` while a `let`/`const` is in its temporal dead zone.
    pub initialized: bool,
    /// A live module import: reads/writes redirect to `(exporter scope, local name)`.
    pub import_ref: Option<(Env, String)>,
    /// `true` for a `var`/function binding created by a sloppy `eval` (CreateMutableBinding with
    /// `deletable` set): `delete <name>` may remove it, unlike ordinary declarations.
    pub deletable: bool,
    /// For an *immutable* binding (`mutable == false`): whether it was created strict. A `const`
    /// (CreateImmutableBinding(_, true)) always throws on reassignment; a named function
    /// expression's own name (CreateImmutableBinding(_, false)) is a silent no-op in sloppy code
    /// and throws only under strict mode. Irrelevant when `mutable` is true.
    pub strict_immutable: bool,
}

impl Binding {
    pub fn data(value: Value, mutable: bool, initialized: bool) -> Binding {
        Binding {
            value,
            mutable,
            initialized,
            import_ref: None,
            deletable: false,
            // An immutable binding created through this helper (const/TDZ) is strict by default.
            strict_immutable: !mutable,
        }
    }
}

pub fn new_scope(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent,
        with_obj: None,
        var_boundary: false,
        catch_param: false,
        lexical_names: Vec::new(),
    }))
}

/// A *variable* environment: the hoisting target for `var`/function declarations (function body,
/// global, or `eval` scope). See [`Scope::var_boundary`].
pub fn new_var_scope(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent,
        with_obj: None,
        var_boundary: true,
        catch_param: false,
        lexical_names: Vec::new(),
    }))
}

/// A `catch (e)` parameter environment: like a block scope, but flagged so a sloppy direct `eval`'s
/// var-hoisting walk skips it (see [`Scope::catch_param`]).
pub fn new_catch_scope(parent: Env) -> Env {
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent: Some(parent),
        with_obj: None,
        var_boundary: false,
        catch_param: true,
        lexical_names: Vec::new(),
    }))
}

/// A `with (obj)` environment: identifier lookups consult `obj` before the enclosing scope.
pub fn new_with_scope(parent: Env, obj: Value) -> Env {
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent: Some(parent),
        with_obj: Some(obj),
        var_boundary: false,
        catch_param: false,
        lexical_names: Vec::new(),
    }))
}

/// Walk up from `env` to the nearest variable environment (function body, global, or `eval` scope) —
/// the scope a `var`/function declaration hoists into. Falls back to the outermost scope.
pub fn nearest_var_env(env: &Env) -> Env {
    let mut cur = env.clone();
    loop {
        if cur.borrow().var_boundary {
            return cur;
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return cur,
        }
    }
}

/// A non-local completion. Expressions only raise `Throw`; the rest flow out of statements.
pub enum Abrupt {
    Throw(Value),
    Return(Value),
    /// Break/Continue carry the completion value threaded so far by the enclosing statement list
    /// (`Value::Empty` when none), per the spec's UpdateEmpty bookkeeping.
    Break(Option<String>, Value),
    Continue(Option<String>, Value),
}

pub type Completion = Result<Value, Abrupt>;

/// UpdateEmpty: replace an EMPTY statement-completion value with `undefined`.
pub(crate) fn update_empty(v: Value) -> Value {
    match v {
        Value::Empty => Value::Undefined,
        other => other,
    }
}

/// UpdateEmpty for an abrupt completion: fill an empty break/continue value with `v`.
pub(crate) fn update_abrupt_empty(a: Abrupt, v: Value) -> Abrupt {
    match a {
        Abrupt::Break(l, Value::Empty) => Abrupt::Break(l, v),
        Abrupt::Continue(l, Value::Empty) => Abrupt::Continue(l, v),
        other => other,
    }
}

thread_local! {
    /// The GlobalSymbolRegistry (`Symbol.for`): shared by every realm (incl. ShadowRealms).
    static SYM_FOR: std::cell::RefCell<HashMap<String, Rc<crate::value::SymbolData>>> =
        RefCell::new(HashMap::new());
}

/// Reset the `Symbol.for` registry (a fresh Engine starts a fresh agent, but ShadowRealms and
/// synthesized realms inside one engine keep sharing it).
pub(crate) fn sym_for_reset() {
    SYM_FOR.with(|m| m.borrow_mut().clear());
}

/// Look up a `Symbol.for` registry entry.
pub(crate) fn sym_for_get(key: &str) -> Option<Rc<crate::value::SymbolData>> {
    SYM_FOR.with(|m| m.borrow().get(key).cloned())
}

/// Register a `Symbol.for` symbol.
pub(crate) fn sym_for_insert(key: String, sym: Rc<crate::value::SymbolData>) {
    SYM_FOR.with(|m| {
        m.borrow_mut().insert(key, sym);
    });
}

/// Whether `sym` is a registered `Symbol.for` symbol (Symbol.keyFor's check).
pub(crate) fn sym_for_contains(sym: &Rc<crate::value::SymbolData>) -> bool {
    SYM_FOR.with(|m| m.borrow().values().any(|r| Rc::ptr_eq(r, sym)))
}

/// The registry key of a registered symbol, if any.
pub(crate) fn sym_for_key_of(sym: &Rc<crate::value::SymbolData>) -> Option<String> {
    SYM_FOR.with(|m| {
        m.borrow()
            .iter()
            .find(|(_, r)| Rc::ptr_eq(r, sym))
            .map(|(k, _)| k.clone())
    })
}

/// Extract the thrown value from an abrupt completion (non-throw completions surface as undefined).
pub fn abrupt_value(a: Abrupt) -> Value {
    match a {
        Abrupt::Throw(v) => v,
        _ => Value::Undefined,
    }
}

/// How [`Interp::bind_pattern`] should bind the identifiers it reaches.
#[derive(Clone, Copy)]
pub enum BindMode {
    /// `var` — assign to the (already-hoisted) function-scoped binding.
    Var,
    /// `let`/`const` — create a fresh lexical binding (`true` = const).
    Lexical(bool),
}

/// Top-level lexically-declared names of a block body (`let`/`const`/`class`) — used by Annex B.3.3
/// to decide whether a synthesized block-function var binding would conflict.
fn block_lexical_names(stmts: &[Stmt]) -> Vec<String> {
    let mut out = Vec::new();
    for s in stmts {
        match s {
            Stmt::VarDecl {
                kind: DeclKind::Let | DeclKind::Const,
                decls,
            } => {
                for (pat, _) in decls {
                    pattern_idents(pat, &mut out);
                }
            }
            Stmt::ClassDecl(class) => {
                if let Some(n) = &class.name {
                    out.push(n.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// BoundNames of a formal parameter list.
pub(crate) fn param_bound_names(params: &[Param]) -> Vec<String> {
    let mut out = Vec::new();
    for p in params {
        pattern_idents(&p.pattern, &mut out);
    }
    out
}

/// Push `added` names onto the Annex B blocked list (dedup'd); returns how many were pushed so
/// the caller can truncate back when leaving the scope.
fn push_blocked(blocked: &mut Vec<String>, added: Vec<String>) -> usize {
    let mut pushed = 0;
    for x in added {
        if !blocked.iter().any(|b| b == &x) {
            blocked.push(x);
            pushed += 1;
        }
    }
    pushed
}

/// Unwrap an `export <decl>` / `export default <decl>` to the declaration it wraps (so the hoisting
/// and lexical-declaration passes treat them like ordinary declarations).
pub fn unwrap_export(stmt: &Stmt) -> &Stmt {
    match stmt {
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => inner,
        other => other,
    }
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

/// Whether a formal parameter list "contains an expression" (ECMAScript ContainsExpression): a
/// default initializer or a destructuring pattern with a default/computed key. When true, the callee
/// gets a separate parameter Environment Record distinct from the body's variable environment, so a
/// direct `eval` in a parameter default cannot leak bindings into the body (and `arguments`/params
/// live in a scope the body's `var` hoisting sits below).
pub fn params_have_expr(params: &[Param]) -> bool {
    params
        .iter()
        .any(|p| p.default.is_some() || pattern_has_expr(&p.pattern))
}

fn pattern_has_expr(pat: &Pattern) -> bool {
    match pat {
        Pattern::Ident(_) | Pattern::Member(_) => false,
        Pattern::Array(elems) => elems.iter().any(|e| match e {
            ArrayPatElem::Hole => false,
            ArrayPatElem::Elem { pattern, default } => {
                default.is_some() || pattern_has_expr(pattern)
            }
            ArrayPatElem::Rest(p) => pattern_has_expr(p),
        }),
        Pattern::Object(o) => o.props.iter().any(|p| {
            p.default.is_some()
                || matches!(p.key, PropKey::Computed(_))
                || pattern_has_expr(&p.value)
        }),
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
    /// The `import.meta` object for the module currently executing (None in script code).
    pub import_meta: Option<Value>,
    /// Default referrer for a bare `import()` in script code (so relative specifiers resolve).
    pub import_base: String,
    /// Loaded module namespace objects, keyed by canonical specifier (for `import()` + caching).
    pub modules: std::collections::HashMap<String, Value>,
    /// Full module records (parsed body, environment, resolved export tables, evaluation status),
    /// keyed by canonical specifier. Drives the two-phase Instantiate/Evaluate module linking.
    pub(crate) module_recs: std::collections::HashMap<String, crate::modules::ModuleRec>,
    /// Host module loader: maps `(specifier, referrer)` → `(canonical_key, source)`.
    #[allow(clippy::type_complexity)]
    pub module_loader: Option<Rc<dyn Fn(&str, &str) -> Option<(String, String)>>>,
    /// Live module-namespace state keyed by the namespace object's pointer: for each exported name,
    /// how to read its current value (a live binding in some module scope, or a static value for a
    /// star-as namespace re-export). Namespace property reads consult this so they stay live.
    pub module_ns: HashMap<usize, HashMap<String, crate::modules::NsBinding>>,
    /// Backing store for Map/Set/WeakMap/WeakSet instances (ordered entries), keyed by the object's
    /// pointer — the engine analogue of an internal `[[MapData]]` slot.
    pub map_data: HashMap<usize, Vec<(Value, Value)>>,
    /// Prototypes for builtins created after `new()` (Map/Set/Date/...), looked up by name so their
    /// native constructors can stamp the right `[[Prototype]]`.
    pub extra_protos: HashMap<&'static str, Gc>,
    /// ArrayBuffer byte storage, keyed by the ArrayBuffer object's pointer.
    pub array_buffers: HashMap<usize, Vec<u8>>,
    /// SharedArrayBuffer pointers → their global shared-memory id (`array_buffers` keeps a
    /// same-length placeholder so detach/length checks still work; the bytes live in the registry).
    pub shared_buffers: HashMap<usize, u64>,
    /// Immutable ArrayBuffer pointers (created via `transferToImmutable`/`sliceToImmutable`): their
    /// bytes can be read but never written, resized, detached, or transferred.
    pub immutable_buffers: std::collections::HashSet<usize>,
    /// Whether this agent may block in `Atomics.wait` (false for the main agent, true for the
    /// worker agents spawned by `$262.agent.start`).
    pub can_block: bool,
    /// Pending `Atomics.waitAsync` operations: each carries the result promise and a channel that a
    /// waiter thread sends "ok"/"timed-out" on. The event loop resolves them as they complete.
    pub pending_async_waits: Vec<(Value, std::sync::mpsc::Receiver<&'static str>)>,
    /// Host timers from `$262.agent.setTimeout`: (callback, deadline).
    pub pending_timers: Vec<(Value, std::time::Instant)>,
    /// Agent-harness wiring (present only in spawned agents / a main with agents).
    pub agent: Option<Box<AgentChannels>>,
    /// TypedArray view state, keyed by the typed-array object's pointer.
    pub typed_arrays: HashMap<usize, TaInfo>,
    /// Object pointers of async generator instances (the AsyncGenerator brand).
    pub async_gens: std::collections::HashSet<usize>,
    /// The global environment's [[VarNames]]: names declared by `var`/function in global code, for
    /// GlobalDeclarationInstantiation's cross-script clash checks.
    pub global_var_names: std::collections::HashSet<String>,

    /// GC pins: one `Gc` clone per object that has an entry in a pointer-keyed side table
    /// (typed_arrays, promises, array_buffers, …). The pin keeps the object from being freed by
    /// plain refcounting — which would let a later allocation reuse its address and inherit the
    /// stale side-table entry — so such objects die only in `gc_collect`'s sweep, which evicts
    /// their table entries first. The collector discounts pins when finding roots.
    pub gc_pins: HashMap<usize, Gc>,
    /// The backing ArrayBuffer *object* for each TypedArray (so the `buffer` getter can return it
    /// without storing it as an observable own property). Keyed by the TypedArray's pointer.
    pub ta_buffer: HashMap<usize, Value>,
    /// Each `ShadowRealm` instance owns an isolated realm (a full sub-interpreter), keyed by the
    /// ShadowRealm object's pointer. Only primitive completion values cross the boundary.
    pub shadow_realms: HashMap<usize, Box<Interp>>,
    /// DataView state `(buffer ptr, byteOffset, byteLength)`, keyed by the DataView's pointer.
    /// DataView state: (buffer ptr, byteOffset, byteLength, is-length-tracking).
    pub data_views: HashMap<usize, (usize, usize, usize, bool)>,
    /// Compiled regular expressions, keyed by the RegExp object's pointer.
    pub regexps: HashMap<usize, Rc<crate::regex::Regex>>,
    /// Proxy `(target, handler)` pairs, keyed by the proxy object's pointer.
    pub proxies: HashMap<usize, (Value, Value)>,
    /// The active `new.target` for the function currently executing (Undefined outside a `new`).
    pub new_target: Value,
    /// The `new.target` to install for the next constructor invocation (set by `construct`).
    pub pending_new_target: Value,
    /// The `$262.IsHTMLDDA` objects, one per realm (emulate `document.all`): typeof "undefined",
    /// falsy, and loosely equal to undefined/null, despite being callable Objects.
    pub htmldda: Vec<Gc>,
    /// The caller's realm during a cross-realm [[Construct]]: the spec pops the callee context
    /// before throwing a derived constructor's return/`this` validation errors, so those errors
    /// belong to the caller's realm.
    pub(crate) ctor_caller_realm: Option<RealmState>,
    /// Additional realms created via `$262.createRealm()`, keyed by the realm's global-object pointer.
    /// Each holds its own intrinsics; the shared side tables (proxies, buffers, …) and well-known
    /// symbols are common, so objects cross realm boundaries freely.
    pub realms: HashMap<usize, RealmState>,
    /// Promise state keyed by the promise object's pointer.
    pub promises: HashMap<usize, PromiseState>,
    /// Temporal object internal slots, keyed by the object's pointer.
    pub temporal: HashMap<usize, crate::temporal::Temporal>,
    /// The calendar id of a Temporal date-bearing object (default "iso8601"), keyed by object ptr.
    pub temporal_cal: HashMap<usize, std::rc::Rc<str>>,
    /// The microtask queue (drained after the main script by [`crate::Engine::eval`]).
    pub microtasks: std::collections::VecDeque<Job>,
    /// Live generator coroutines, keyed by the generator object's pointer. Each owns an OS thread
    /// that runs the body and parks at every `yield` (see [`crate::coroutine`]).
    pub generators: HashMap<usize, crate::coroutine::Coroutine>,
    /// Live-object count above which the next allocation safe point runs the cycle collector.
    pub gc_next: i64,
    /// True while a native constructor is being invoked via `new` (lets e.g. `Number`/`String`
    /// build a wrapper object instead of returning a primitive).
    pub constructing: bool,
    /// True only while a *derived* class constructor body is executing — the one context where a
    /// `super(...)` call is legal. Field initializers, methods, plain functions and global code all
    /// clear it, so a stray `super()` (including one reached through a direct `eval`) is rejected
    /// instead of re-entering instance-field initialization unboundedly.
    pub super_call_ok: bool,
    /// Set by `yield*` just before parking: the yielded value is the inner iterator's result
    /// object and must pass through the generator driver unwrapped.
    pub yield_raw_result: bool,
    /// True while statements *directly* in an async generator's body run (nested ordinary
    /// functions clear it; arrows inherit): `return <expr>` awaits its value only there.
    pub in_async_gen_body: bool,
    /// NamedEvaluation: the binding/property name an anonymous class expression being evaluated
    /// should take — applied *before* its static initializers run.
    pub pending_fn_name: Option<String>,
    /// A pending proper tail call: set by `return f(...)` in a strict ordinary function; the
    /// nearest `Interp::call` frame re-dispatches it (a trampoline), keeping the stack flat.
    pub pending_tail: Option<(Value, Value, Vec<Value>)>,
    /// True while executing a body where `return f(...)` is a proper tail call (a strict,
    /// ordinary, non-constructor function).
    pub tco_ok: bool,
    /// GetTemplateObject cache: one frozen strings array per tagged-template *site* (AST node),
    /// so the same site always passes the identical object. Keyed by the quasis slice address.
    pub template_cache: std::collections::HashMap<usize, Value>,
    /// True while class field initializer code runs: a direct eval from there may not contain an
    /// `arguments` reference (arrows inherit the context; ordinary functions clear it).
    pub in_field_init_code: bool,
    /// A stack of disposal scopes — one frame per block/function body that can hold `using`
    /// declarations. Resources are disposed in reverse on scope exit (see `dispose_frame`).
    pub using_stack: Vec<Vec<Disposable>>,
    /// Monotonic counter minting globally-unique private backing keys for auto-accessors (so a
    /// subclass's accessor never collides with a superclass's on the same instance).
    pub accessor_seq: u64,
    /// Scratch collector for `context.addInitializer(fn)` calls during decorator application; drained
    /// after each decorator runs.
    pub decorator_initializers: Vec<Value>,
    /// Annex B.3.3 web-compat function hoisting: block/if-position function declarations (in sloppy
    /// code) whose names got a function-scope `var` binding, keyed by AST pointer. When such a
    /// declaration is *evaluated*, the block binding's value is copied into the var binding. The
    /// `Rc` value keeps the AST alive so a pointer is never reused by a different function.
    pub annexb_fn_sync: HashMap<usize, Rc<Function>>,
    /// `import defer` namespaces awaiting first access: namespace object ptr → module key.
    /// Accessing a property of one evaluates the module (then the entry is removed).
    pub(crate) deferred_ns: HashMap<usize, String>,
    /// module key → its (single) deferred namespace object.
    pub(crate) deferred_ns_objs: HashMap<String, Value>,
    /// Promise-state forwarding: when a native super() grafts a fresh promise's state onto the
    /// subclass `this`, the original object's resolvers redirect here.
    pub promise_forward: HashMap<usize, Value>,
    /// Mapped `arguments` objects: object ptr → (function scope, per-index parameter name — None
    /// once unmapped by delete/defineProperty). Reads/writes of still-mapped indices alias the
    /// parameter bindings.
    pub(crate) mapped_arguments: HashMap<usize, (Env, Vec<Option<String>>)>,
    /// Source-phase imports: canonical module key → its (cached) ModuleSource object.
    pub(crate) module_source_objs: HashMap<String, Value>,
    /// FinalizationRegistry registrations (ptr → unregister tokens), for `unregister`'s result.
    pub(crate) fr_tokens: HashMap<usize, Vec<Value>>,
    /// Async generators mid-step (running or parked at an `await`): further next/return/throw
    /// requests queue here (the spec's AsyncGeneratorRequest queue) until the step completes.
    pub(crate) async_gen_busy: std::collections::HashSet<usize>,
    pub(crate) async_gen_queue:
        HashMap<usize, std::collections::VecDeque<(Value, crate::coroutine::Resume)>>,
}

/// A `using x = v` resource: the value plus its captured dispose method.
pub struct Disposable {
    pub value: Value,
    pub method: Value,
    /// The method came from `@@asyncDispose` — its result is awaited during disposal.
    pub method_is_async: bool,
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
    /// Instance fields (and auto-accessor backing fields), in declaration order.
    pub fields: Vec<FieldInit>,
    /// The environment field initializers evaluate in (carries the class's super bindings).
    pub field_env: Env,
    /// True if the class has an `extends` clause (derived: `this` is set up by `super()`).
    pub derived: bool,
    /// Instance initializers registered by decorators via `context.addInitializer`, run on each new
    /// instance after its fields are set, with `this` = the instance.
    pub instance_initializers: Vec<Value>,
    /// Instance private methods and accessors, stamped as own properties on each instance when
    /// `this` is created (before the fields run). Stamping twice is a TypeError.
    pub private_members: Vec<(String, crate::value::Property)>,
}

/// One instance field: its key, optional initializer expression, and any decorator-supplied
/// initializer functions (each maps the current value to a new one during construction).
pub struct FieldInit {
    pub key: String,
    pub init: Option<Expr>,
    pub transforms: Vec<Value>,
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

/// A realm's intrinsics: the global object, its environment, and the per-realm prototypes/constructors
/// installed by `builtins::install`. Well-known symbols and the engine side tables are shared, not here.
pub struct RealmState {
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
    pub eval_fn: Option<Gc>,
    pub extra_protos: HashMap<&'static str, Gc>,
}

impl Interp {
    /// Snapshot the current realm's intrinsics so they can be swapped out and back.
    fn snapshot_realm(&self) -> RealmState {
        RealmState {
            global: self.global.clone(),
            global_env: self.global_env.clone(),
            object_proto: self.object_proto.clone(),
            function_proto: self.function_proto.clone(),
            array_proto: self.array_proto.clone(),
            string_proto: self.string_proto.clone(),
            number_proto: self.number_proto.clone(),
            boolean_proto: self.boolean_proto.clone(),
            symbol_proto: self.symbol_proto.clone(),
            error_protos: self.error_protos.clone(),
            eval_fn: self.eval_fn.clone(),
            extra_protos: self.extra_protos.clone(),
        }
    }

    /// Install a realm's intrinsics as the active ones.
    fn restore_realm(&mut self, r: &RealmState) {
        self.global = r.global.clone();
        self.global_env = r.global_env.clone();
        self.object_proto = r.object_proto.clone();
        self.function_proto = r.function_proto.clone();
        self.array_proto = r.array_proto.clone();
        self.string_proto = r.string_proto.clone();
        self.number_proto = r.number_proto.clone();
        self.boolean_proto = r.boolean_proto.clone();
        self.symbol_proto = r.symbol_proto.clone();
        self.error_protos = r.error_protos.clone();
        self.eval_fn = r.eval_fn.clone();
        self.extra_protos = r.extra_protos.clone();
    }

    /// `$262.createRealm()`: build a fresh realm (its own global + intrinsics) and register it. The
    /// well-known symbols are shared with the creating realm so `@@iterator` etc. match cross-realm.
    pub fn create_realm(&mut self) -> Value {
        // Register the creating (main) realm too, so cross-realm dispatch can switch BACK to it
        // and resolve its intrinsics like any other realm's.
        if self.realms.is_empty() {
            let main = self.snapshot_realm();
            self.realms.insert(Rc::as_ptr(&main.global) as usize, main);
        }
        let saved = self.snapshot_realm();
        let saved_iter = self.iterator_sym.clone();
        // The main realm's well-known symbols, to graft onto the new realm's Symbol constructor.
        let well_known: Vec<(&'static str, Value)> = WELL_KNOWN_SYMBOLS
            .iter()
            .filter_map(|name| {
                let sym = self
                    .get_member(&Value::Obj(saved.global.clone()), "Symbol")
                    .ok()?;
                let v = self.get_member(&sym, name).ok()?;
                Some((*name, v))
            })
            .collect();

        // Fresh intrinsics, mirroring `Interp::new`.
        let object_proto = Object::new(None);
        let function_proto = Object::new(Some(object_proto.clone()));
        let array_proto = Object::new(Some(object_proto.clone()));
        let string_proto = Object::new(Some(object_proto.clone()));
        let number_proto = Object::new(Some(object_proto.clone()));
        let boolean_proto = Object::new(Some(object_proto.clone()));
        string_proto.borrow_mut().exotic = Exotic::StrWrap(Rc::from(""));
        number_proto.borrow_mut().exotic = Exotic::NumWrap(0.0);
        boolean_proto.borrow_mut().exotic = Exotic::BoolWrap(false);
        let symbol_proto = Object::new(Some(object_proto.clone()));
        let global = Object::new(Some(object_proto.clone()));
        self.object_proto = object_proto;
        self.function_proto = function_proto;
        self.array_proto = array_proto;
        self.string_proto = string_proto;
        self.number_proto = number_proto;
        self.boolean_proto = boolean_proto;
        self.symbol_proto = symbol_proto;
        self.global = global.clone();
        self.global_env = new_var_scope(None);
        self.error_protos = HashMap::new();
        self.extra_protos = HashMap::new();
        self.eval_fn = None;
        crate::builtins::install(self);
        // Top-level `this` is the new global.
        let g = Value::Obj(self.global.clone());
        self.global_env.borrow_mut().vars.insert(
            "this".to_string(),
            Binding {
                value: g,
                mutable: false,
                strict_immutable: true,
                initialized: true,
                import_ref: None,
                deletable: false,
            },
        );
        // Share the well-known symbols: overwrite the realm's freshly-minted ones with the originals.
        if let Ok(Value::Obj(rs)) = self.get_member(&Value::Obj(self.global.clone()), "Symbol") {
            for (name, sym) in &well_known {
                rs.borrow_mut()
                    .props
                    .insert(*name, Property::data(sym.clone(), false, false, false));
            }
        }
        self.iterator_sym = saved_iter;

        // Tag this realm's `eval` so an *indirect* call to it (`otherRealm.eval(code)`) runs the code
        // in this realm's global scope, not the caller's (see `call_inner`).
        if let Some(ef) = &self.eval_fn {
            ef.borrow_mut().props.insert(
                "__eval_realm",
                Property::data(Value::Obj(self.global.clone()), false, false, false),
            );
        }

        let realm_state = self.snapshot_realm();
        let ptr = Rc::as_ptr(&global) as usize;
        self.realms.insert(ptr, realm_state);
        self.restore_realm(&saved);
        Value::Obj(global)
    }

    /// Run `src` as a script in the realm whose global is `realm_global`.
    pub fn eval_in_realm(&mut self, realm_global: &Value, src: &str) -> Result<Value, Abrupt> {
        let ptr = match realm_global {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return Err(self.throw("TypeError", "not a realm global")),
        };
        let realm = match self.realms.get(&ptr) {
            Some(r) => r.snapshot_clone(),
            None => return Err(self.throw("TypeError", "unknown realm")),
        };
        let saved = self.snapshot_realm();
        self.restore_realm(&realm);
        let env = self.global_env.clone();
        // Indirect eval semantics, in the target realm's global scope.
        let result = self.perform_eval(src, &env, false);
        self.restore_realm(&saved);
        result
    }
}

impl RealmState {
    fn snapshot_clone(&self) -> RealmState {
        RealmState {
            global: self.global.clone(),
            global_env: self.global_env.clone(),
            object_proto: self.object_proto.clone(),
            function_proto: self.function_proto.clone(),
            array_proto: self.array_proto.clone(),
            string_proto: self.string_proto.clone(),
            number_proto: self.number_proto.clone(),
            boolean_proto: self.boolean_proto.clone(),
            symbol_proto: self.symbol_proto.clone(),
            error_protos: self.error_protos.clone(),
            eval_fn: self.eval_fn.clone(),
            extra_protos: self.extra_protos.clone(),
        }
    }
}

/// The well-known symbol names that are shared across all realms.
const WELL_KNOWN_SYMBOLS: &[&str] = &[
    "iterator",
    "asyncIterator",
    "hasInstance",
    "isConcatSpreadable",
    "match",
    "matchAll",
    "replace",
    "search",
    "species",
    "split",
    "toPrimitive",
    "toStringTag",
    "unscopables",
    "dispose",
    "asyncDispose",
    "metadata",
];

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
        let global_env = new_var_scope(None);
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
            console: Vec::new(),
            strict: false,
            depth: 0,
            class_info: HashMap::new(),
            eval_fn: None,
            iterator_sym: None,
            short_circuit: false,
            import_meta: None,
            import_base: String::new(),
            modules: std::collections::HashMap::new(),
            module_recs: std::collections::HashMap::new(),
            module_loader: None,
            module_ns: HashMap::new(),
            map_data: HashMap::new(),
            extra_protos: HashMap::new(),
            array_buffers: HashMap::new(),
            shared_buffers: HashMap::new(),
            immutable_buffers: std::collections::HashSet::new(),
            can_block: true,
            pending_async_waits: Vec::new(),
            pending_timers: Vec::new(),
            agent: None,
            typed_arrays: HashMap::new(),
            async_gens: std::collections::HashSet::new(),
            global_var_names: std::collections::HashSet::new(),
            gc_pins: HashMap::new(),
            ta_buffer: HashMap::new(),
            shadow_realms: HashMap::new(),
            data_views: HashMap::new(),
            regexps: HashMap::new(),
            proxies: HashMap::new(),
            new_target: Value::Undefined,
            pending_new_target: Value::Undefined,
            htmldda: Vec::new(),
            ctor_caller_realm: None,
            realms: HashMap::new(),
            promises: HashMap::new(),
            temporal: HashMap::new(),
            temporal_cal: HashMap::new(),
            microtasks: std::collections::VecDeque::new(),
            generators: HashMap::new(),
            gc_next: GC_TRIGGER,
            constructing: false,
            super_call_ok: false,
            in_field_init_code: false,
            yield_raw_result: false,
            in_async_gen_body: false,
            pending_fn_name: None,
            pending_tail: None,
            tco_ok: false,
            template_cache: std::collections::HashMap::new(),
            using_stack: Vec::new(),
            accessor_seq: 0,
            decorator_initializers: Vec::new(),
            annexb_fn_sync: HashMap::new(),
            deferred_ns: HashMap::new(),
            deferred_ns_objs: HashMap::new(),
            promise_forward: HashMap::new(),
            mapped_arguments: HashMap::new(),
            module_source_objs: HashMap::new(),
            fr_tokens: HashMap::new(),
            async_gen_busy: std::collections::HashSet::new(),
            async_gen_queue: HashMap::new(),
        };
        crate::builtins::install(&mut interp);
        // `this` at the top level is the global object (sloppy mode).
        let g = Value::Obj(interp.global.clone());
        interp.global_env.borrow_mut().vars.insert(
            "this".to_string(),
            Binding {
                value: g,
                mutable: false,
                strict_immutable: true,
                initialized: true,
                import_ref: None,
                deletable: false,
            },
        );
        // Register the main realm so a call back into main-realm code from inside another realm
        // swaps the main intrinsics back in (see `callee_realm_global`).
        let main = interp.snapshot_realm();
        interp
            .realms
            .insert(Rc::as_ptr(&interp.global) as usize, main);
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
            obj.borrow_mut()
                .props
                .insert("message", Property::builtin(Value::from_string(msg)));
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
        if idx >= self.ta_len(info).unwrap_or(0) {
            return Value::Undefined;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let decode = |buf: &[u8]| -> Value {
            if start + es <= buf.len() {
                let bytes = &buf[start..start + es];
                if info.kind.is_bigint() {
                    Value::BigInt(info.kind.read_bigint(bytes))
                } else {
                    Value::Num(info.kind.read(bytes))
                }
            } else {
                Value::Undefined
            }
        };
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            if let Some(mem) = shared_mem_get(id) {
                return decode(&mem.lock().unwrap());
            }
            return Value::Undefined;
        }
        match self.array_buffers.get(&info.buffer) {
            Some(buf) => decode(buf),
            _ => Value::Undefined,
        }
    }

    /// Atomically read-modify-write an integer TypedArray element: `f` maps the old raw value to
    /// the new one (or `None` for no write, as in a failed compareExchange). For a shared buffer
    /// the whole operation happens under one lock hold, so concurrent agents can't interleave
    /// (a lost `Atomics.add` increment would spin another agent's waitUntil loop forever).
    /// Returns the old value, or `None` when the index is out of range.
    pub fn ta_modify(
        &mut self,
        info: &TaInfo,
        idx: usize,
        f: impl FnOnce(i128) -> Option<i128>,
    ) -> Option<i128> {
        if idx >= self.ta_len(info).unwrap_or(0) {
            return None;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let apply = |buf: &mut [u8]| -> Option<i128> {
            if start + es > buf.len() {
                return None;
            }
            let bytes = &buf[start..start + es];
            let old = if info.kind.is_bigint() {
                info.kind.read_bigint(bytes)
            } else {
                info.kind.read(bytes) as i128
            };
            if let Some(new) = f(old) {
                let nb = if info.kind.is_bigint() {
                    info.kind.write_bigint(new)
                } else {
                    info.kind.write(new as f64)
                };
                buf[start..start + es].copy_from_slice(&nb);
            }
            Some(old)
        };
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            if let Some(mem) = shared_mem_get(id) {
                let mut buf = mem.lock().unwrap();
                return apply(&mut buf);
            }
            return None;
        }
        match self.array_buffers.get_mut(&info.buffer) {
            Some(buf) => apply(buf),
            None => None,
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
        if idx >= self.ta_len(info).unwrap_or(0) {
            return;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let bytes = info.kind.write_bigint(n);
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            if let Some(mem) = shared_mem_get(id) {
                let mut buf = mem.lock().unwrap();
                if start + es <= buf.len() {
                    buf[start..start + es].copy_from_slice(&bytes);
                }
            }
            return;
        }
        if let Some(buf) = self.array_buffers.get_mut(&info.buffer) {
            if start + es <= buf.len() {
                buf[start..start + es].copy_from_slice(&bytes);
            }
        }
    }

    /// Write Number `n` into element `idx` of a TypedArray (out-of-range writes are ignored).
    pub fn ta_write(&mut self, info: &TaInfo, idx: usize, n: f64) {
        if idx >= self.ta_len(info).unwrap_or(0) {
            return;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let bytes = info.kind.write(n);
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            if let Some(mem) = shared_mem_get(id) {
                let mut buf = mem.lock().unwrap();
                if start + es <= buf.len() {
                    buf[start..start + es].copy_from_slice(&bytes);
                }
            }
            return;
        }
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
        let data = Rc::new(SymbolData {
            id: self.sym_counter,
            description,
        });
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
    /// Whether `key` is an internal private-element key. Every runtime private name carries a
    /// `\u{1}<serial>` suffix (auto-accessor backings a `\u{0}` marker), so a user property whose
    /// *string* name merely starts with `#` (a computed key) is not mistaken for one.
    pub fn is_private_key(key: &str) -> bool {
        key.starts_with('#') && (key.contains('\u{1}') || key.contains('\u{0}'))
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
            b.props.insert(
                "length",
                Property::data(Value::Num(len as f64), true, false, false),
            );
        }
        Value::Obj(obj)
    }

    /// Build the generator/iterator object whose `next`/`return`/`throw` drive its coroutine (stored
    /// separately in `self.generators`).
    fn make_generator(&mut self, is_async: bool, gen_proto: Option<Value>) -> Value {
        // GetPrototypeFromConstructor: the instance's [[Prototype]] is the generator function's
        // `.prototype` (which chains to %GeneratorPrototype% / %AsyncGeneratorPrototype%, where
        // next/return/throw and @@asyncIterator live), else the intrinsic directly.
        let intrinsic = if is_async {
            "%AsyncGeneratorPrototype%"
        } else {
            "%GeneratorPrototype%"
        };
        let proto = match gen_proto {
            Some(Value::Obj(o)) => Some(o),
            _ => self.extra_protos.get(intrinsic).cloned(),
        }
        .or_else(|| Some(self.object_proto.clone()));
        Value::Obj(Object::new(proto))
    }

    pub fn make_native(&self, name: &str, len: usize, f: NativeFn) -> Gc {
        let obj = Object::new(Some(self.function_proto.clone()));
        {
            let mut b = obj.borrow_mut();
            b.call = Callable::Native(f);
            b.props.insert(
                "length",
                Property::data(Value::Num(len as f64), false, false, true),
            );
            b.props.insert(
                "name",
                Property::data(Value::from_string(name.to_string()), false, false, true),
            );
        }
        obj
    }

    /// Define a native method on `target` (non-enumerable, as built-ins are).
    pub fn def_method(&self, target: &Gc, name: &str, len: usize, f: NativeFn) {
        let func = self.make_native(name, len, f);
        target
            .borrow_mut()
            .props
            .insert(name, Property::builtin(Value::Obj(func)));
    }

    pub fn make_function(&self, func: Rc<Function>, env: Env) -> Value {
        let is_arrow = func.is_arrow;
        let is_method = func.is_method;
        let is_generator = func.is_generator;
        let is_async = func.is_async;
        // A generator / async / async-generator function object's [[Prototype]] is the matching
        // intrinsic (%GeneratorFunction.prototype% etc.), not %Function.prototype%. Async arrows
        // use %AsyncFunction.prototype% too.
        let fn_proto = match (is_arrow, is_async, is_generator) {
            (false, false, true) => self.extra_protos.get("%GeneratorFunction.prototype%"),
            (_, true, false) => self.extra_protos.get("%AsyncFunction.prototype%"),
            (false, true, true) => self.extra_protos.get("%AsyncGeneratorFunction.prototype%"),
            _ => None,
        }
        .cloned()
        .unwrap_or_else(|| self.function_proto.clone());
        let obj = Object::new(Some(fn_proto));
        let arity = func
            .params
            .iter()
            .take_while(|p| p.default.is_none() && !p.rest)
            .count();
        let name = func.name.clone().unwrap_or_default();
        {
            let mut b = obj.borrow_mut();
            b.call = Callable::User(func, env);
            b.props.insert(
                "length",
                Property::data(Value::Num(arity as f64), false, false, true),
            );
            b.props.insert(
                "name",
                Property::data(Value::from_string(name), false, false, true),
            );
        }
        // A `prototype` is present on ordinary functions and on generators — sync OR async (even
        // generator methods); arrows, plain async functions, and concise methods/getters/setters have
        // none. A generator's `.prototype` chains to %GeneratorPrototype% / %AsyncGeneratorPrototype%
        // (an ordinary function's is a plain object). Only ordinary functions are constructors.
        let has_prototype = !is_arrow && (is_generator || (!is_async && !is_method));
        if has_prototype {
            let proto_parent = match (is_generator, is_async) {
                (true, false) => self.extra_protos.get("%GeneratorPrototype%").cloned(),
                (true, true) => self.extra_protos.get("%AsyncGeneratorPrototype%").cloned(),
                _ => None,
            }
            .or_else(|| Some(self.object_proto.clone()));
            let proto = Object::new(proto_parent);
            // A generator's `.prototype` has no own `constructor` (the methods live on the intrinsic).
            if !is_generator {
                proto
                    .borrow_mut()
                    .props
                    .insert("constructor", Property::builtin(Value::Obj(obj.clone())));
            }
            obj.borrow_mut().props.insert(
                "prototype",
                Property::data(Value::Obj(proto), true, false, false),
            );
        }
        if !is_arrow && !is_method && !is_async && !is_generator {
            obj.borrow_mut().is_constructor = true;
        }
        Value::Obj(obj)
    }

    // ----- property access --------------------------------------------------------------------

    /// Get `base[key]`, walking the prototype chain and invoking getters. Primitive bases are
    /// handled by routing to their wrapper prototype (and string index/length specially). The
    /// receiver defaults to `base`.
    pub fn get_member(&mut self, base: &Value, key: &str) -> Result<Value, Abrupt> {
        self.get_member_recv(base, key, base.clone())
    }

    /// [[Get]](P, Receiver): like [`get_member`] but with an explicit `receiver` — the `this` a
    /// getter is invoked with, the proxy `get` trap's Receiver argument, and what a forwarded
    /// `[[Get]]` carries through a proxy target chain.
    /// The deferred-namespace evaluation trigger: most operations with a string key (and all
    /// key-less ones like [[OwnPropertyKeys]]) evaluate the module; symbol keys and the "then"
    /// property never do. Accessing a namespace whose module is mid-evaluation is a TypeError.
    pub(crate) fn defer_trigger(&mut self, o: &Gc, key: Option<&str>) -> Result<(), Abrupt> {
        if self.deferred_ns.is_empty() {
            return Ok(());
        }
        if matches!(key, Some(k) if Interp::is_sym_key(k) || k == "then" || Interp::is_private_key(k))
        {
            return Ok(());
        }
        let module_key = match self.deferred_ns.get(&(Rc::as_ptr(o) as usize)) {
            Some(k) => k.clone(),
            None => return Ok(()),
        };
        self.evaluate_deferred(&module_key)
    }

    pub fn get_member_recv(
        &mut self,
        base: &Value,
        key: &str,
        receiver: Value,
    ) -> Result<Value, Abrupt> {
        // An `import defer` namespace evaluates its module on string-keyed access.
        if let Value::Obj(o) = base {
            self.defer_trigger(o, Some(key))?;
        }
        match base {
            Value::Undefined | Value::Empty | Value::Null => Err(self.throw(
                "TypeError",
                format!("cannot read property '{key}' of {}", type_name(base)),
            )),
            Value::Str(s) => {
                // `length` and indices are in UTF-16 code units.
                if key == "length" {
                    return Ok(Value::Num(crate::jstr::unit_len(s) as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(match crate::jstr::UnitIter::new(s).nth(i) {
                        Some(u) => Value::from_string(crate::jstr::unit_str(u)),
                        None => Value::Undefined,
                    });
                }
                let proto = self.string_proto.clone();
                self.get_from_chain(&proto, key, &receiver)
            }
            Value::Num(_) => {
                let proto = self.number_proto.clone();
                self.get_from_chain(&proto, key, &receiver)
            }
            Value::Bool(_) => {
                let proto = self.boolean_proto.clone();
                self.get_from_chain(&proto, key, &receiver)
            }
            Value::Sym(s) => {
                if key == "description" {
                    return Ok(s
                        .description
                        .clone()
                        .map(Value::Str)
                        .unwrap_or(Value::Undefined));
                }
                let proto = self.symbol_proto.clone();
                self.get_from_chain(&proto, key, &receiver)
            }
            Value::BigInt(_) => match self.extra_protos.get("BigInt").cloned() {
                Some(proto) => self.get_from_chain(&proto, key, &receiver),
                None => Ok(Value::Undefined),
            },
            Value::Obj(o) => {
                let o = o.clone();
                let ptr = Rc::as_ptr(&o) as usize;
                // A mapped `arguments` index aliases its parameter binding.
                if !self.mapped_arguments.is_empty() {
                    if let Some(name) = self.mapped_arg_name(ptr, key) {
                        if let Some((env, _)) = self.mapped_arguments.get(&ptr) {
                            let v = env.borrow().vars.get(&name).map(|b| b.value.clone());
                            if let Some(v) = v {
                                return Ok(v);
                            }
                        }
                    }
                }
                // String wrapper (`new String(...)`/`Object("...")`): own indexed chars + `length`.
                if let Exotic::StrWrap(s) = o.borrow().exotic.clone() {
                    if key == "length" {
                        return Ok(Value::Num(crate::jstr::unit_len(&s) as f64));
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        if let Some(u) = crate::jstr::UnitIter::new(&s).nth(i) {
                            return Ok(Value::from_string(crate::jstr::unit_str(u)));
                        }
                    }
                }
                // Module namespace: exports read live from the exporting module's scope (or a stored
                // static value for a star-as namespace re-export).
                if !self.module_ns.is_empty() {
                    if let Some(map) = self.module_ns.get(&ptr) {
                        if let Some(binding) = map.get(key) {
                            match binding.clone() {
                                crate::modules::NsBinding::Live(mod_env, local) => {
                                    return self.get_var(&local, &mod_env);
                                }
                                crate::modules::NsBinding::Static(v) => return Ok(v),
                            }
                        }
                    }
                }
                // Proxy: invoke the `get` trap, or forward to the target.
                if !self.proxies.is_empty() {
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        let trap = self.get_member(&handler, "get")?;
                        if matches!(trap, Value::Undefined | Value::Null) {
                            // Forward to the target's [[Get]], preserving the original Receiver.
                            return self.get_member_recv(&target, key, receiver);
                        }
                        if !trap.is_callable() {
                            return Err(self.throw("TypeError", "proxy 'get' trap is not callable"));
                        }
                        let res = self.call(
                            trap,
                            handler,
                            &[
                                target.clone(),
                                self.sym_from_key(key).unwrap_or_else(|| Value::str(key)),
                                receiver.clone(),
                            ],
                        )?;
                        self.proxy_get_invariant(&target, key, &res)?;
                        return Ok(res);
                    }
                }
                // TypedArray integer-index reads come from the backing buffer, not the property map.
                // length/byteLength/byteOffset are computed (and 0 once the buffer is detached).
                if let Some(info) = self.typed_arrays.get(&ptr).copied() {
                    match self.ta_index_kind(&info, key) {
                        TaIndex::Element(idx) => return Ok(self.ta_read(&info, idx)),
                        // A canonical-numeric non-index never reaches the prototype: it reads undefined.
                        TaIndex::Exotic => return Ok(Value::Undefined),
                        TaIndex::Ordinary => {}
                    }
                    // The meta keys live on %TypedArray.prototype% as accessors; an own property
                    // (defineProperty on the instance) shadows them.
                    let cur = self.ta_len(&info);
                    if o.borrow().props.contains(key) {
                        return self.get_from_chain(&o, key, &receiver);
                    }
                    match key {
                        "length" => return Ok(Value::Num(cur.unwrap_or(0) as f64)),
                        "byteLength" => {
                            return Ok(Value::Num((cur.unwrap_or(0) * info.kind.elsize()) as f64))
                        }
                        "byteOffset" => {
                            return Ok(Value::Num(if cur.is_none() {
                                0.0
                            } else {
                                info.offset as f64
                            }))
                        }
                        "BYTES_PER_ELEMENT" => return Ok(Value::Num(info.kind.elsize() as f64)),
                        "buffer" => {
                            return Ok(self
                                .ta_buffer
                                .get(&ptr)
                                .cloned()
                                .unwrap_or(Value::Undefined))
                        }
                        _ => {}
                    }
                }
                self.get_from_chain(&o, key, &receiver)
            }
        }
    }

    fn get_from_chain(&mut self, start: &Gc, key: &str, receiver: &Value) -> Result<Value, Abrupt> {
        let mut cur = Some(start.clone());
        while let Some(obj) = cur {
            // A deferred namespace anywhere on the chain evaluates its module first.
            self.defer_trigger(&obj, Some(key))?;
            // A proxy anywhere on the chain handles the read itself (with the original receiver).
            let ptr = Rc::as_ptr(&obj) as usize;
            if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                if matches!(handler, Value::Null) {
                    return Err(self.throw("TypeError", "cannot perform 'get' on a revoked proxy"));
                }
                let trap = self.get_member(&handler, "get")?;
                if matches!(trap, Value::Undefined | Value::Null) {
                    return self.get_member_recv(&target, key, receiver.clone());
                }
                if !trap.is_callable() {
                    return Err(self.throw("TypeError", "proxy 'get' trap is not callable"));
                }
                let res = self.call(
                    trap,
                    handler,
                    &[
                        target.clone(),
                        self.sym_from_key(key).unwrap_or_else(|| Value::str(key)),
                        receiver.clone(),
                    ],
                )?;
                self.proxy_get_invariant(&target, key, &res)?;
                return Ok(res);
            }
            let prop = obj.borrow().props.get(key).cloned();
            if let Some(p) = prop {
                if p.accessor {
                    // Legacy `fn.caller` / `fn.arguments`: reading the poisoned
                    // %Function.prototype% accessor through an ordinary sloppy function
                    // yields undefined instead of throwing.
                    if matches!(key, "caller" | "arguments")
                        && self.is_throw_type_error(&p.get)
                        && !Rc::ptr_eq(
                            &obj,
                            match receiver {
                                Value::Obj(r) => r,
                                _ => &obj,
                            },
                        )
                    {
                        if let Value::Obj(r) = receiver {
                            if let Callable::User(f, _) = &r.borrow().call {
                                if !f.is_strict && !f.is_arrow && !f.is_generator && !f.is_async {
                                    return Ok(Value::Undefined);
                                }
                            }
                        }
                    }
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

    /// Throw with the constructing caller's realm's error intrinsics (see `ctor_caller_realm`).
    fn throw_in_caller_realm(&mut self, kind: &str, msg: &str) -> Abrupt {
        match self.ctor_caller_realm.take() {
            Some(caller) => {
                let cur = self.snapshot_realm();
                self.restore_realm(&caller);
                let e = self.throw(kind, msg);
                self.restore_realm(&cur);
                self.ctor_caller_realm = Some(caller);
                e
            }
            None => self.throw(kind, msg),
        }
    }

    /// Whether `v` is the [[IsHTMLDDA]] object.
    pub(crate) fn is_htmldda(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => self.htmldda.iter().any(|d| Rc::ptr_eq(o, d)),
            _ => false,
        }
    }

    /// Whether `getter` is the %ThrowTypeError% intrinsic.
    fn is_throw_type_error(&self, getter: &Option<Value>) -> bool {
        match (getter, self.extra_protos.get("%ThrowTypeError%")) {
            (Some(Value::Obj(g)), Some(tte)) => Rc::ptr_eq(g, tte),
            _ => false,
        }
    }

    /// Set `base[key] = value`, honouring setters, accessor-only properties, read-only data
    /// properties, and array `length`/index bookkeeping. The receiver defaults to `base`.
    /// The parameter name a still-mapped `arguments[key]` aliases, if any.
    pub(crate) fn mapped_arg_name(&self, ptr: usize, key: &str) -> Option<String> {
        let (_, names) = self.mapped_arguments.get(&ptr)?;
        let idx: usize = key.parse().ok()?;
        names.get(idx)?.clone()
    }

    /// The current parameter value a mapped `arguments[key]` aliases, if the map is live.
    pub(crate) fn mapped_arg_value(&self, ptr: usize, key: &str) -> Option<Value> {
        let name = self.mapped_arg_name(ptr, key)?;
        let (env, _) = self.mapped_arguments.get(&ptr)?;
        env.borrow().vars.get(&name).map(|b| b.value.clone())
    }

    /// Write through a mapped `arguments[key]` alias into its parameter binding.
    pub(crate) fn mapped_arg_write(&mut self, ptr: usize, key: &str, v: Value) -> bool {
        let Some(name) = self.mapped_arg_name(ptr, key) else {
            return false;
        };
        let Some((env, _)) = self.mapped_arguments.get(&ptr) else {
            return false;
        };
        if let Some(b) = env.borrow_mut().vars.get_mut(&name) {
            b.value = v;
            return true;
        }
        false
    }

    /// Unmap an `arguments` index (delete / defineProperty severs the parameter alias).
    pub(crate) fn unmap_argument(&mut self, ptr: usize, key: &str) {
        if let Some((_, names)) = self.mapped_arguments.get_mut(&ptr) {
            if let Ok(idx) = key.parse::<usize>() {
                if let Some(slot) = names.get_mut(idx) {
                    *slot = None;
                }
            }
        }
    }

    /// Resolve a module-namespace property before rejecting a write: an exported binding still in
    /// its temporal dead zone throws ReferenceError (per the namespace [[GetOwnProperty]] step).
    fn ns_probe_tdz(&mut self, ptr: usize, key: &str) -> Result<(), Abrupt> {
        let live = self
            .module_ns
            .get(&ptr)
            .and_then(|map| map.get(key))
            .and_then(|b| match b {
                crate::modules::NsBinding::Live(env, local) => Some((env.clone(), local.clone())),
                _ => None,
            });
        if let Some((env, local)) = live {
            self.get_var(&local, &env)?;
        }
        Ok(())
    }

    pub fn set_member(&mut self, base: &Value, key: &str, value: Value) -> Result<(), Abrupt> {
        self.set_member_recv(base, key, value, base.clone())
            .map(|_| ())
    }

    /// [[Set]](P, V, Receiver): like [`set_member`] but with an explicit `receiver` and returning the
    /// [[Set]] success boolean. The receiver is the object a setter is invoked on, the proxy `set`
    /// trap's Receiver argument, and what a forwarded `[[Set]]` carries through a proxy target chain.
    pub fn set_member_recv(
        &mut self,
        base: &Value,
        key: &str,
        value: Value,
        receiver: Value,
    ) -> Result<bool, Abrupt> {
        // A mapped `arguments` index aliases its parameter binding: writes update the parameter
        // (and the own property, so unmapped reads and enumeration stay consistent).
        if !self.mapped_arguments.is_empty() {
            if let Value::Obj(o) = base {
                let ptr = Rc::as_ptr(o) as usize;
                if let Some(name) = self.mapped_arg_name(ptr, key) {
                    if let Some((env, _)) = self.mapped_arguments.get(&ptr) {
                        let env = env.clone();
                        if let Some(b) = env.borrow_mut().vars.get_mut(&name) {
                            b.value = value.clone();
                        }
                        // Update the own property's value in place — its attributes (e.g. a
                        // configurable:false from defineProperty) must survive.
                        let mut b = o.borrow_mut();
                        if let Some(p) = b.props.get_mut(key) {
                            p.value = value;
                        } else {
                            b.props
                                .insert(key, crate::value::Property::data(value, true, true, true));
                        }
                        return Ok(true);
                    }
                }
            }
        }
        let obj = match base {
            Value::Obj(o) => o.clone(),
            Value::Undefined | Value::Null => {
                return Err(self.throw(
                    "TypeError",
                    format!("cannot set property '{key}' of {}", type_name(base)),
                ))
            }
            // Setting a property on a primitive: an accessor (or proxy) on the wrapper
            // prototype chain still handles the write, with the primitive as receiver; otherwise
            // the write fails — a silent no-op in sloppy mode, a TypeError in strict mode.
            _ => {
                let proto = match base {
                    Value::Str(_) => Some(self.string_proto.clone()),
                    Value::Num(_) => Some(self.number_proto.clone()),
                    Value::Bool(_) => Some(self.boolean_proto.clone()),
                    Value::Sym(_) => Some(self.symbol_proto.clone()),
                    Value::BigInt(_) => self.extra_protos.get("BigInt").cloned(),
                    _ => None,
                };
                let mut cur = proto;
                while let Some(o) = cur {
                    // A proxy on the chain (or any accessor) takes over with the original receiver.
                    if self.proxies.contains_key(&(Rc::as_ptr(&o) as usize)) {
                        return self.set_member_recv(&Value::Obj(o), key, value, receiver);
                    }
                    let prop = o.borrow().props.get(key).cloned();
                    if let Some(p) = prop {
                        if p.accessor {
                            return match p.set {
                                Some(setter) => {
                                    self.call(setter, receiver.clone(), &[value])?;
                                    Ok(true)
                                }
                                None => {
                                    if self.strict {
                                        Err(self.throw(
                                            "TypeError",
                                            format!("cannot set getter-only property '{key}'"),
                                        ))
                                    } else {
                                        Ok(false)
                                    }
                                }
                            };
                        }
                        break;
                    }
                    let parent = o.borrow().proto.clone();
                    cur = parent;
                }
                if self.strict {
                    return Err(self.throw(
                        "TypeError",
                        format!(
                            "Cannot create property '{key}' on {} value",
                            type_name(base)
                        ),
                    ));
                }
                return Ok(false);
            }
        };

        let ptr = Rc::as_ptr(&obj) as usize;
        // A module namespace exotic object's [[Set]] always fails: in strict code (all module code
        // is strict) the assignment throws a TypeError; a sloppy caller sees an inert no-op. The
        // property is resolved first, so a binding still in its TDZ throws ReferenceError instead.
        if self.is_namespace(ptr) {
            self.ns_probe_tdz(ptr, key)?;
            if self.strict {
                return Err(self.throw(
                    "TypeError",
                    format!(
                        "Cannot assign to read only property '{key}' of module namespace object"
                    ),
                ));
            }
            return Ok(false);
        }
        // Proxy: invoke the `set` trap, or forward to the target.
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                let trap = self.get_member(&handler, "set")?;
                if matches!(trap, Value::Undefined | Value::Null) {
                    // Forward to the target's [[Set]], preserving the original Receiver.
                    return self.set_member_recv(&target, key, value, receiver);
                }
                if !trap.is_callable() {
                    return Err(self.throw("TypeError", "proxy 'set' trap is not callable"));
                }
                let ok = self.call(
                    trap,
                    handler,
                    &[
                        target.clone(),
                        self.sym_from_key(key).unwrap_or_else(|| Value::str(key)),
                        value.clone(),
                        receiver.clone(),
                    ],
                )?;
                // A successful `set` can't contradict a non-configurable property on the target.
                let success = self.to_boolean(&ok);
                if success {
                    self.proxy_set_invariant(&target, key, &value)?;
                }
                return Ok(success);
            }
        }
        // TypedArray integer-index writes go straight to the backing buffer; a canonical-numeric key
        // that isn't a valid index is inert (the value coercion still runs, then is discarded).
        if let Some(info) = self.typed_arrays.get(&ptr).copied() {
            match self.ta_index_kind(&info, key) {
                TaIndex::Element(_) | TaIndex::Exotic => {
                    // IntegerIndexedElementSet coerces the value first — the coercion can resize
                    // the underlying buffer, changing which indices are valid — then re-checks the
                    // index and silently discards an out-of-bounds write.
                    let num = if info.kind.is_bigint() {
                        Value::BigInt(self.to_bigint(&value)?)
                    } else {
                        Value::Num(self.to_number(&value)?)
                    };
                    if let TaIndex::Element(idx) = self.ta_index_kind(&info, key) {
                        self.ta_store(&info, idx, &num)?;
                    }
                    return Ok(true);
                }
                TaIndex::Ordinary => {}
            }
        }

        // Walk the chain for an accessor or read-only data property.
        let mut cur = Some(obj.clone());
        while let Some(o) = cur {
            // A deferred namespace anywhere on the chain evaluates its module first.
            self.defer_trigger(&o, Some(key))?;
            // A proxy on the chain handles the write itself (with the original receiver).
            let optr = Rc::as_ptr(&o) as usize;
            if let Some((target, handler)) = self.proxies.get(&optr).cloned() {
                if matches!(handler, Value::Null) {
                    return Err(self.throw("TypeError", "cannot perform 'set' on a revoked proxy"));
                }
                let trap = self.get_member(&handler, "set")?;
                if matches!(trap, Value::Undefined | Value::Null) {
                    return self.set_member_recv(&target, key, value, receiver);
                }
                if !trap.is_callable() {
                    return Err(self.throw("TypeError", "proxy 'set' trap is not callable"));
                }
                let ok = self.call(
                    trap,
                    handler,
                    &[
                        target.clone(),
                        self.sym_from_key(key).unwrap_or_else(|| Value::str(key)),
                        value.clone(),
                        receiver.clone(),
                    ],
                )?;
                let success = self.to_boolean(&ok);
                if success {
                    self.proxy_set_invariant(&target, key, &value)?;
                }
                return Ok(success);
            }
            // A TypedArray reached via the prototype chain: its integer-indexed elements are its own
            // properties, so the search must stop here (never consulting the TA prototype's
            // accessors). A valid index is a writable data property → create one on the receiver; a
            // canonical-numeric non-index is an inert success.
            if !Rc::ptr_eq(&o, &obj) {
                if let Some(info) = self.typed_arrays.get(&optr).copied() {
                    match self.ta_index_kind(&info, key) {
                        TaIndex::Element(_) | TaIndex::Exotic if matches!(&receiver, Value::Obj(r) if Rc::ptr_eq(r, &o)) =>
                        {
                            // The receiver IS this TypedArray: IntegerIndexedElementSet applies
                            // (coerce, then store or silently drop an out-of-range write).
                            let num = if info.kind.is_bigint() {
                                Value::BigInt(self.to_bigint(&value)?)
                            } else {
                                Value::Num(self.to_number(&value)?)
                            };
                            if let TaIndex::Element(idx) = self.ta_index_kind(&info, key) {
                                self.ta_store(&info, idx, &num)?;
                            }
                            return Ok(true);
                        }
                        // Foreign receiver: a valid element behaves as a writable data property
                        // (create it on the receiver, uncoerced); a numeric non-index is inert.
                        TaIndex::Element(_) => break,
                        TaIndex::Exotic => return Ok(true),
                        TaIndex::Ordinary => {}
                    }
                }
            }
            let prop = o.borrow().props.get(key).cloned();
            if let Some(p) = prop {
                if p.accessor {
                    return match p.set {
                        Some(setter) => {
                            self.call(setter, receiver.clone(), &[value])?;
                            Ok(true)
                        }
                        None => {
                            if self.strict {
                                Err(self.throw(
                                    "TypeError",
                                    format!("cannot set getter-only property '{key}'"),
                                ))
                            } else {
                                Ok(false)
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
                        return Ok(false);
                    }
                    break; // own writable data property — update below
                }
                if !p.writable {
                    if self.strict {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot assign to read-only property '{key}'"),
                        ));
                    }
                    return Ok(false);
                }
                break; // inherited writable data property — create own on receiver
            }
            cur = o.borrow().proto.clone();
        }

        // OrdinarySet: the write lands on the *receiver* (they differ for `super.x = v` and
        // Reflect.set with an explicit receiver). A namespace receiver rejects the define,
        // probing the binding first so a TDZ export surfaces ReferenceError.
        let obj = match &receiver {
            Value::Obj(r) if !Rc::ptr_eq(r, &obj) => {
                // A deferred-namespace receiver evaluates before its own properties are consulted.
                self.defer_trigger(r, Some(key))?;
                let rptr = Rc::as_ptr(r) as usize;
                if self.is_namespace(rptr) {
                    self.ns_probe_tdz(rptr, key)?;
                    if self.strict {
                        return Err(self.throw(
                            "TypeError",
                            format!(
                                "Cannot assign to read only property '{key}' of module namespace object"
                            ),
                        ));
                    }
                    return Ok(false);
                }
                // A proxy receiver gets the write as CreateDataProperty through its
                // [[DefineOwnProperty]] trap.
                if self.proxies.contains_key(&rptr) {
                    return match crate::builtins::reflect_define_on_receiver(
                        self, &receiver, key, value,
                    ) {
                        Ok(false) if self.strict => Err(self.throw(
                            "TypeError",
                            format!("cannot create property '{key}' on the receiver"),
                        )),
                        Ok(b) => Ok(b),
                        Err(v) => Err(Abrupt::Throw(v)),
                    };
                }
                r.clone()
            }
            _ => obj,
        };
        let is_array = matches!(obj.borrow().exotic, Exotic::Array);
        if is_array {
            return self.array_set(&obj, key, value);
        } else {
            let existed = obj.borrow().props.contains(key);
            if existed {
                if let Some(p) = obj.borrow_mut().props.get_mut(key) {
                    p.value = value;
                }
            } else {
                if !obj.borrow().extensible {
                    if self.strict {
                        return Err(self
                            .throw("TypeError", "cannot add property, object is not extensible"));
                    }
                    return Ok(false);
                }
                obj.borrow_mut().props.insert(key, Property::plain(value));
            }
        }
        Ok(true)
    }

    fn array_set(&mut self, obj: &Gc, key: &str, value: Value) -> Result<bool, Abrupt> {
        if key == "length" {
            // ArraySetLength coerces twice (ToUint32 then ToNumber, both observable) and
            // RangeErrors on a mismatch before any writability check.
            let n1 = self.to_number(&value)?;
            let new_u: u32 = if n1.is_nan() || n1.is_infinite() || n1 == 0.0 {
                0
            } else {
                (n1.trunc() as i64 as u64 & 0xFFFF_FFFF) as u32
            };
            let n = self.to_number(&value)?;
            let same = if n == 0.0 {
                new_u == 0
            } else {
                (new_u as f64) == n
            };
            if !same {
                return Err(self.throw("RangeError", "Invalid array length"));
            }
            // A non-writable `length` (frozen/sealed array) rejects the change — re-read after
            // the coercions, which may have frozen it.
            let len_writable = obj
                .borrow()
                .props
                .get("length")
                .map(|p| p.writable)
                .unwrap_or(true);
            if !len_writable {
                if self.strict {
                    return Err(
                        self.throw("TypeError", "cannot assign to read-only property 'length'")
                    );
                }
                return Ok(false);
            }
            let new_len = n as usize;
            let old_len = self.array_length(obj);
            if new_len < old_len {
                // Delete out-of-range indices from the top down, stopping at a non-configurable one
                // (ArraySetLength). Collect + sort descending so the scan is O(n log n), never O(n²).
                let mut indices: Vec<usize> = obj
                    .borrow()
                    .props
                    .keys()
                    .iter()
                    .filter_map(|k| k.parse::<usize>().ok())
                    .filter(|&idx| idx >= new_len)
                    .collect();
                indices.sort_unstable_by(|a, b| b.cmp(a));
                for idx in indices {
                    let configurable = obj
                        .borrow()
                        .props
                        .get(&idx.to_string())
                        .map(|p| p.configurable)
                        .unwrap_or(true);
                    if configurable {
                        obj.borrow_mut().props.remove(&idx.to_string());
                    } else {
                        // length settles just past the blocking element; a strict assignment throws.
                        obj.borrow_mut().props.insert(
                            "length",
                            Property::data(Value::Num((idx + 1) as f64), true, false, false),
                        );
                        if self.strict {
                            return Err(self.throw(
                                "TypeError",
                                "cannot delete non-configurable array element via length",
                            ));
                        }
                        return Ok(false);
                    }
                }
            }
            obj.borrow_mut().props.insert(
                "length",
                Property::data(Value::Num(new_len as f64), true, false, false),
            );
            return Ok(true);
        }
        // Adding a new index to a non-extensible (sealed/frozen) array is rejected.
        if !obj.borrow().props.contains(key) && !obj.borrow().extensible {
            if self.strict {
                return Err(
                    self.throw("TypeError", "cannot add property, object is not extensible")
                );
            }
            return Ok(false);
        }
        // Appending past a non-writable `length` is rejected.
        if let Some(idx) = key.parse::<u64>().ok().filter(|&i| i < 4294967295) {
            let len = self.array_length(obj);
            let len_writable = obj
                .borrow()
                .props
                .get("length")
                .map(|p| p.writable)
                .unwrap_or(true);
            if idx as usize >= len && !len_writable {
                if self.strict {
                    return Err(self.throw(
                        "TypeError",
                        "cannot add an element past a read-only array length",
                    ));
                }
                return Ok(false);
            }
        }
        // An existing element keeps its attributes; [[Set]] only updates the value.
        if obj.borrow().props.contains(key) {
            if let Some(p) = obj.borrow_mut().props.get_mut(key) {
                p.value = value;
            }
        } else {
            obj.borrow_mut().props.insert(key, Property::plain(value));
        }
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
        Ok(true)
    }

    /// A TypedArray's *current* element length, or `None` if it's out of bounds (a fixed-length view
    /// whose resizable buffer shrank below its range) or its buffer is detached. A length-tracking
    /// view recomputes its length from the buffer's current size.
    pub fn ta_len(&self, info: &TaInfo) -> Option<usize> {
        let buflen = self.array_buffers.get(&info.buffer)?.len();
        let es = info.kind.elsize();
        if info.track {
            if info.offset > buflen {
                None
            } else {
                Some((buflen - info.offset) / es)
            }
        } else if info.offset + info.len * es > buflen {
            None
        } else {
            Some(info.len)
        }
    }

    /// Classify a property key against a TypedArray's integer-index exotic behavior: a valid
    /// in-range element index, a *canonical numeric* key that isn't one (e.g. "1.5", "-0", "-1",
    /// out of range — these are inert: never stored, never reach the prototype), or an ordinary key.
    pub fn ta_index_kind(&self, info: &TaInfo, key: &str) -> TaIndex {
        let n = match self.canonical_numeric_index(key) {
            Some(n) => n,
            None => return TaIndex::Ordinary,
        };
        // Valid index: a non-negative integer (not -0) below the current length.
        if n >= 0.0
            && n.fract() == 0.0
            && !(n == 0.0 && n.is_sign_negative())
            && (n as usize) < self.ta_len(info).unwrap_or(0)
        {
            TaIndex::Element(n as usize)
        } else {
            TaIndex::Exotic
        }
    }

    /// CanonicalNumericIndexString: the numeric value when `key` is the canonical string form of a
    /// Number ("0", "1.5", "-0", "Infinity", "NaN"…), else None.
    pub fn canonical_numeric_index(&self, key: &str) -> Option<f64> {
        if key == "-0" {
            return Some(-0.0);
        }
        let n = match key {
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            "NaN" => f64::NAN,
            _ => key.parse::<f64>().ok()?,
        };
        if self.num_to_str(n) == key {
            Some(n)
        } else {
            None
        }
    }

    pub fn array_length(&self, obj: &Gc) -> usize {
        // A TypedArray's length lives in its info slot, not an own `length` property.
        if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(obj) as usize)) {
            return self.ta_len(info).unwrap_or(0);
        }
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

    /// Pin `o` for the lifetime of its side-table entries (see `gc_pins`).
    pub(crate) fn gc_pin(&mut self, o: &Gc) {
        self.gc_pins.insert(Rc::as_ptr(o) as usize, o.clone());
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

        // A pin is bookkeeping, not a real holder: count it like an internal reference so a
        // pinned-but-unreachable object is still collectable (the sweep evicts its entries).
        for o in self.gc_pins.values() {
            let b = o.borrow();
            b.gc_internal.set(b.gc_internal.get() + 1);
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
                self.ta_buffer.remove(&ptr);
                self.shared_buffers.remove(&ptr);
                self.immutable_buffers.remove(&ptr);
                self.generators.remove(&ptr);
                self.async_gens.remove(&ptr);
                self.async_gen_busy.remove(&ptr);
                self.async_gen_queue.remove(&ptr);
                self.mapped_arguments.remove(&ptr);
                self.deferred_ns.remove(&ptr);
                self.promise_forward.remove(&ptr);
                self.gc_pins.remove(&ptr);
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
        let mut r = self.call_inner(callee, this, args);
        // Trampoline: a proper tail call unwound out of the callee re-dispatches here, in the
        // same stack frame, so mutual tail recursion runs in constant stack space.
        while r.is_ok() {
            match self.pending_tail.take() {
                Some((f, t, a)) => {
                    if let Err(e) = self.gc_check() {
                        r = Err(e);
                        break;
                    }
                    r = self.call_inner(f, t, &a);
                }
                None => break,
            }
        }
        self.depth -= 1;
        r
    }

    /// GetFunctionRealm: the realm-global pointer of `obj`, unwrapping bound functions and
    /// proxies (a revoked proxy is a TypeError). `None` means the active realm.
    pub(crate) fn get_function_realm_global(&mut self, obj: &Gc) -> Result<Option<usize>, Abrupt> {
        let mut cur = obj.clone();
        for _ in 0..64 {
            if let Some((target, handler)) = self.proxies.get(&(Rc::as_ptr(&cur) as usize)) {
                if matches!(handler, Value::Null) {
                    return Err(self.throw("TypeError", "proxy has been revoked"));
                }
                match target.clone() {
                    Value::Obj(t) => {
                        cur = t;
                        continue;
                    }
                    _ => return Ok(None),
                }
            }
            let bound = match &cur.borrow().call {
                Callable::Bound { target, .. } => Some(target.clone()),
                _ => None,
            };
            match bound {
                Some(t) => cur = t,
                None => return Ok(self.callee_realm_global(&cur)),
            }
        }
        Ok(None)
    }

    fn call_inner(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, Abrupt> {
        // A cross-realm callee runs with its own realm's intrinsics active (so a thrown TypeError,
        // a fresh object's prototype, or a global lookup lands in the right realm).
        if !self.realms.is_empty() {
            if let Value::Obj(o) = &callee {
                // A proxy's trap machinery runs in the caller's realm (the trap-arguments array is
                // a caller-realm Array); invoking the trap function swaps on its own.
                if !self.proxies.contains_key(&(Rc::as_ptr(o) as usize)) {
                    if let Some(gptr) = self.callee_realm_global(o) {
                        let saved = self.snapshot_realm();
                        let target = self.realms[&gptr].snapshot_clone();
                        self.restore_realm(&target);
                        let r = self.call_dispatch(callee, this, args);
                        self.restore_realm(&saved);
                        return r;
                    }
                }
            }
        }
        self.call_dispatch(callee, this, args)
    }

    /// The realm (keyed by its global's pointer) a function belongs to, when it is NOT the active
    /// realm. A user function's realm is the one whose global scope its environment chain is
    /// rooted in; anything else is resolved by finding which realm's %Function.prototype% sits on
    /// its prototype chain.
    pub(crate) fn callee_realm_global(&self, obj: &Gc) -> Option<usize> {
        if let Callable::User(_, env) = &obj.borrow().call {
            let mut root = env.clone();
            loop {
                let parent = root.borrow().parent.clone();
                match parent {
                    Some(p) => root = p,
                    None => break,
                }
            }
            for (g, rs) in &self.realms {
                if Rc::ptr_eq(&root, &rs.global_env) {
                    if Rc::ptr_eq(&rs.global, &self.global) {
                        return None;
                    }
                    return Some(*g);
                }
            }
        }
        let mut cur = obj.borrow().proto.clone();
        let mut hops = 0;
        while let Some(p) = cur {
            if Rc::ptr_eq(&p, &self.function_proto) {
                return None;
            }
            for (g, rs) in &self.realms {
                if Rc::ptr_eq(&p, &rs.function_proto) {
                    if Rc::ptr_eq(&rs.function_proto, &self.function_proto) {
                        return None;
                    }
                    return Some(*g);
                }
            }
            hops += 1;
            if hops > 8 {
                break;
            }
            cur = p.borrow().proto.clone();
        }
        None
    }

    fn call_dispatch(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Abrupt> {
        let obj = match &callee {
            Value::Obj(o) => o.clone(),
            _ => {
                return Err(self.throw(
                    "TypeError",
                    format!("{} is not a function", type_name(&callee)),
                ))
            }
        };
        // Proxy with an `apply` trap (or forward to the target).
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&(Rc::as_ptr(&obj) as usize)).cloned()
            {
                let trap = self.get_member(&handler, "apply")?;
                if matches!(trap, Value::Undefined | Value::Null) {
                    return self.call(target, this, args);
                }
                if !trap.is_callable() {
                    return Err(self.throw("TypeError", "proxy 'apply' trap is not callable"));
                }
                let arr = self.make_array(args.to_vec());
                return self.call(trap, handler, &[target, this, arr]);
            }
        }
        // An indirect call to another realm's `eval` runs in that realm's global scope. (A direct
        // `eval(...)` of the current realm is intercepted earlier, in `eval_call`.)
        if !self.realms.is_empty() {
            let realm_g = obj
                .borrow()
                .props
                .get("__eval_realm")
                .map(|p| p.value.clone());
            if let Some(realm_g @ Value::Obj(_)) = realm_g {
                return match args.first() {
                    Some(Value::Str(s)) => {
                        let s = s.clone();
                        self.eval_in_realm(&realm_g, &s)
                    }
                    Some(other) => Ok(other.clone()),
                    None => Ok(Value::Undefined),
                };
            }
        }

        let call = obj.borrow().call.clone();
        // A plain call is never constructing (only `new` sets the flag). Clearing it here keeps a
        // wrapper constructor invoked as a function — `Number(x)` — from boxing. `new.target` is
        // likewise cleared so a native constructor called *as a function* during an outer `new`
        // (e.g. `new F(){ Function('a','b') }`) doesn't inherit the outer new.target.
        let saved_ctor = self.constructing;
        let saved_nt = self.new_target.clone();
        self.constructing = false;
        // An arrow inherits the enclosing new.target lexically — don't clear it for one.
        if !matches!(&call, Callable::User(f, _) if f.is_arrow) {
            self.new_target = Value::Undefined;
        }
        let r = match call {
            Callable::None => Err(self.throw("TypeError", "value is not a function")),
            Callable::Native(f) => f(self, this, args).map_err(Abrupt::Throw),
            Callable::User(func, env) => {
                // A class constructor cannot be [[Call]]ed.
                if self.class_info.contains_key(&(Rc::as_ptr(&obj) as usize)) {
                    return Err(self.throw(
                        "TypeError",
                        "Class constructor cannot be invoked without 'new'",
                    ));
                }
                self.call_user(&func, env, this, args, false, &obj)
            }
            Callable::Bound {
                target,
                this: bthis,
                args: bargs,
            } => {
                let mut all = bargs.clone();
                all.extend_from_slice(args);
                self.call(Value::Obj(target), bthis, &all)
            }
            Callable::WrappedShadow { realm, target } => {
                self.call_wrapped_shadow(realm, *target, args)
            }
            Callable::WrappedCross {
                realm,
                parent,
                target,
            } => {
                if std::env::var("LUMEN_DBG").is_ok() {
                    eprintln!(
                        "DBG cross enter realm={realm} parent={parent:x} self={:p} tkind={}",
                        self as *const Interp,
                        match &*target {
                            Value::Obj(o) => match &o.borrow().call {
                                Callable::User(..) => "user",
                                Callable::WrappedShadow { .. } => "wshadow",
                                Callable::WrappedCross { .. } => "wcross",
                                Callable::Native(_) => "native",
                                _ => "other",
                            },
                            _ => "nonobj",
                        }
                    );
                }
                // SAFETY: the host Interp (engine root or a boxed sub-realm) is pinned in memory
                // while any of its sub-realm's objects — including this wrapper — exist, and it
                // is suspended (not mutably borrowed) whenever sub-realm code runs.
                let host: &mut Interp = unsafe { &mut *(parent as *mut Interp) };
                let mut out_args = Vec::with_capacity(args.len());
                for a in args {
                    if a.is_callable() {
                        match host.make_wrapped_shadow(realm, a.clone()) {
                            Ok(w) => out_args.push(w),
                            Err(_) => {
                                return Err(self.throw(
                                    "TypeError",
                                    "cannot wrap the callable argument for the host realm",
                                ))
                            }
                        }
                    } else if matches!(a, Value::Obj(_)) {
                        return Err(self.throw(
                            "TypeError",
                            "wrapped function arguments must be primitives or callables",
                        ));
                    } else {
                        out_args.push(a.clone());
                    }
                }
                match host.call(*target, Value::Undefined, &out_args) {
                    Ok(v) if !matches!(v, Value::Obj(_)) => Ok(v),
                    Ok(v) if v.is_callable() => Ok(self.make_wrapped_cross(realm, parent, v)),
                    Ok(_) => {
                        Err(self.throw("TypeError", "a wrapped function returned a non-primitive"))
                    }
                    Err(e) => {
                        if std::env::var("LUMEN_DBG").is_ok() {
                            eprintln!(
                                "DBG cross err: {:?}",
                                match &e {
                                    Abrupt::Throw(Value::Str(s)) => format!("str {s}"),
                                    Abrupt::Throw(Value::Obj(o)) => format!(
                                        "obj {:?}",
                                        o.borrow()
                                            .props
                                            .get("message")
                                            .map(|p| p.value.clone())
                                            .and_then(|v| match v {
                                                Value::Str(s) => Some(s.to_string()),
                                                _ => None,
                                            })
                                    ),
                                    _ => "non-throw".to_string(),
                                }
                            );
                        }
                        Err(self.throw("TypeError", "a wrapped function threw in the host realm"))
                    }
                }
            }
            // Auto-accessor get/set: the receiver must *own* the private backing field (so a static
            // accessor reached through a subclass, which doesn't carry the slot, throws).
            Callable::AccessorGet(key) => self.accessor_load(&this, &key),
            Callable::AccessorSet(key) => self.accessor_store(
                &this,
                &key,
                args.first().cloned().unwrap_or(Value::Undefined),
            ),
            // Decorator `context.access` get/set: read/write a named property on the first argument.
            Callable::PropGet(key) => {
                let recv = args.first().cloned().unwrap_or(Value::Undefined);
                self.get_member(&recv, &key)
            }
            Callable::PropSet(key) => {
                let recv = args.first().cloned().unwrap_or(Value::Undefined);
                let val = args.get(1).cloned().unwrap_or(Value::Undefined);
                self.set_member(&recv, &key, val).map(|_| Value::Undefined)
            }
        };
        self.constructing = saved_ctor;
        self.new_target = saved_nt;
        r
    }

    /// Read an auto-accessor's backing field off `this`, throwing if `this` lacks the slot.
    fn accessor_load(&mut self, this: &Value, key: &str) -> Result<Value, Abrupt> {
        match this {
            Value::Obj(o) if o.borrow().props.contains(key) => {
                Ok(o.borrow().props.get(key).map(|p| p.value.clone()).unwrap())
            }
            _ => Err(self.throw(
                "TypeError",
                "cannot read auto-accessor backing field from an unrelated object",
            )),
        }
    }

    /// Write an auto-accessor's backing field on `this`, throwing if `this` lacks the slot.
    fn accessor_store(&mut self, this: &Value, key: &str, value: Value) -> Result<Value, Abrupt> {
        match this {
            Value::Obj(o) if o.borrow().props.contains(key) => {
                if let Some(p) = o.borrow_mut().props.get_mut(key) {
                    p.value = value;
                }
                Ok(Value::Undefined)
            }
            _ => Err(self.throw(
                "TypeError",
                "cannot write auto-accessor backing field on an unrelated object",
            )),
        }
    }

    /// Make a ShadowRealm wrapped function: a caller-realm callable around `target` in `realm`.
    /// CopyNameAndLength copies the target's `length` (a non-negative integer) and `name` (a string,
    /// else "") — a throwing `length`/`name` getter propagates.
    /// The namespace object of a loaded module (for ShadowRealm.importValue).
    pub(crate) fn module_namespace(&self, key: &str) -> Option<Value> {
        self.module_recs.get(key).map(|m| m.ns.clone())
    }

    /// Marshal an evaluate/importValue result out of the shadow realm `realm`.
    pub(crate) fn make_shadow_result(&mut self, realm: usize, v: Value) -> Result<Value, Abrupt> {
        self.marshal_from_shadow(realm, v)
    }

    /// Marshal one value crossing OUT of a shadow realm into this (host) realm.
    fn marshal_from_shadow(&mut self, realm: usize, v: Value) -> Result<Value, Abrupt> {
        if !matches!(v, Value::Obj(_)) {
            return Ok(v);
        }
        if v.is_callable() {
            return self.make_wrapped_shadow(realm, v);
        }
        Err(self.throw(
            "TypeError",
            "only primitives and callables may cross a ShadowRealm boundary",
        ))
    }

    pub fn make_wrapped_shadow(&mut self, realm: usize, target: Value) -> Result<Value, Abrupt> {
        let f = Object::new(Some(self.function_proto.clone()));
        f.borrow_mut().call = Callable::WrappedShadow {
            realm,
            target: Box::new(target.clone()),
        };
        // CopyNameAndLength: the Gets run in the realm that owns `target` (its proxy state
        // lives there); an abrupt Get is a TypeError of the calling realm.
        let bad = |i: &mut Interp| {
            i.throw(
                "TypeError",
                "wrapped function name/length are not accessible",
            )
        };
        let (length_r, name_r) = match self.shadow_realms.remove(&realm) {
            Some(mut sub) => {
                // HasOwnProperty first (its [[GetOwnProperty]] trap is observable and may throw).
                let l = match crate::builtins::has_own_property_trapped(&mut sub, &target, "length")
                {
                    Ok(true) => sub.get_member(&target, "length"),
                    Ok(false) => Ok(Value::Num(0.0)),
                    Err(e) => Err(Abrupt::Throw(e)),
                };
                let n = sub.get_member(&target, "name");
                self.shadow_realms.insert(realm, sub);
                (l, n)
            }
            None => (
                self.get_member(&target, "length"),
                self.get_member(&target, "name"),
            ),
        };
        let length = match length_r {
            Ok(Value::Num(n)) if n.is_finite() => n.trunc().max(0.0),
            Ok(Value::Num(n)) if n == f64::INFINITY => f64::INFINITY,
            Ok(_) => 0.0,
            Err(_) => return Err(bad(self)),
        };
        let name = match name_r {
            Ok(Value::Str(s)) => s.to_string(),
            Ok(_) => String::new(),
            Err(_) => return Err(bad(self)),
        };
        {
            let mut b = f.borrow_mut();
            b.props.insert(
                "length",
                Property::data(Value::Num(length), false, false, true),
            );
            b.props.insert(
                "name",
                Property::data(Value::from_string(name), false, false, true),
            );
        }
        Ok(Value::Obj(f))
    }

    /// Call a ShadowRealm wrapped function: marshal primitive args into the sub-realm, call `target`
    /// there, and marshal the primitive (or further-wrapped callable) result back.
    fn call_wrapped_shadow(
        &mut self,
        realm: usize,
        target: Value,
        args: &[Value],
    ) -> Result<Value, Abrupt> {
        // The sub-interpreter is used in place (Box targets are pinned), so re-entrant calls —
        // host code invoked from inside the realm calling back into it — work.
        let subptr = match self.shadow_realms.get_mut(&realm) {
            Some(b) => &mut **b as *mut Interp,
            None => return Err(self.throw("TypeError", "the ShadowRealm is no longer available")),
        };
        // SAFETY: pinned Box target; any aliasing re-entry is sequenced by the JS call stack.
        let sub: &mut Interp = unsafe { &mut *subptr };
        let parent_ptr = self as *mut Interp as usize;
        // Primitive arguments cross directly; callables wrap as sub-realm functions that
        // re-enter this realm; other objects throw.
        let mut inner_args = Vec::with_capacity(args.len());
        for a in args {
            if a.is_callable() {
                inner_args.push(sub.make_wrapped_cross(realm, parent_ptr, a.clone()));
            } else if matches!(a, Value::Obj(_)) {
                return Err(self.throw(
                    "TypeError",
                    "ShadowRealm wrapped function: arguments must be primitives or callables",
                ));
            } else {
                inner_args.push(a.clone());
            }
        }
        let result = {
            let r = sub.call(target, Value::Undefined, &inner_args);
            sub.drain_microtasks();
            r
        };
        match result {
            Ok(v) => self
                .marshal_from_shadow(realm, v)
                .map_err(|_| {
                    abrupt_value(
                        self.throw("TypeError", "a wrapped function returned a non-primitive"),
                    )
                })
                .map_err(Abrupt::Throw),
            Err(e) => {
                if std::env::var("LUMEN_DBG").is_ok() {
                    eprintln!(
                        "DBG cws err: {:?}",
                        match &e {
                            Abrupt::Throw(Value::Str(s)) => format!("str {s}"),
                            Abrupt::Throw(Value::Obj(o)) => format!(
                                "obj {:?} {:?}",
                                o.borrow()
                                    .props
                                    .get("name")
                                    .map(|p| p.value.clone())
                                    .and_then(|v| match v {
                                        Value::Str(s) => Some(s.to_string()),
                                        _ => None,
                                    }),
                                o.borrow()
                                    .props
                                    .get("message")
                                    .map(|p| p.value.clone())
                                    .and_then(|v| match v {
                                        Value::Str(s) => Some(s.to_string()),
                                        _ => None,
                                    })
                            ),
                            _ => "non-throw".to_string(),
                        }
                    );
                }
                Err(self.throw(
                    "TypeError",
                    "a wrapped function threw inside the ShadowRealm",
                ))
            }
        }
    }

    /// Create the sub-realm-side wrapper for a host callable.
    pub(crate) fn make_wrapped_cross(
        &mut self,
        realm: usize,
        parent: usize,
        target: Value,
    ) -> Value {
        let f = Object::new(Some(self.function_proto.clone()));
        f.borrow_mut().call = Callable::WrappedCross {
            realm,
            parent,
            target: Box::new(target),
        };
        f.borrow_mut().props.insert(
            "length",
            Property::data(Value::Num(0.0), false, false, true),
        );
        f.borrow_mut()
            .props
            .insert("name", Property::data(Value::str(""), false, false, true));
        Value::Obj(f)
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
        // A function with parameter expressions (default values or destructuring with defaults) gets
        // a separate parameter Environment Record — not a variable environment — so its body's `var`
        // hoisting sits in a distinct scope below it (and a direct `eval` in a parameter default
        // cannot leak declarations into the body). Otherwise a single variable environment suffices.
        // A named function *expression*'s name binds immutably inside the function; a
        // *declaration*'s name already has a (mutable) binding in the enclosing scope, so it must
        // NOT get the self-reference (that would make `function f(){ f = 1 }` a silent no-op —
        // and an Annex B block function's binding may have been reassigned between calls).
        let self_ref_needed = func.is_fn_expr && func.name.is_some();
        // A named function expression's self-name binds in its own environment *outside* the
        // variable environment, so a body-level `var` of the same name creates a fresh binding
        // instead of aliasing the callee.
        let closure = if self_ref_needed {
            let selfref_env = new_scope(Some(closure));
            if let Some(name) = &func.name {
                selfref_env.borrow_mut().vars.insert(
                    name.clone(),
                    Binding {
                        value: Value::Obj(fn_obj.clone()),
                        mutable: false,
                        initialized: true,
                        import_ref: None,
                        deletable: false,
                        // Non-strict immutable: reassignment is a silent no-op in sloppy code.
                        strict_immutable: false,
                    },
                );
            }
            selfref_env
        } else {
            closure
        };
        let has_param_exprs = params_have_expr(&func.params);
        let scope = if has_param_exprs {
            // Chain: callee base (variable env) → parameter env → body variable env. A direct `eval`
            // in a parameter default hoists its `var`s into the callee base, not the enclosing
            // scope, and its walk passes through the parameter env (where `arguments`/params live).
            let callee_base = new_var_scope(Some(closure));
            new_scope(Some(callee_base))
        } else {
            new_var_scope(Some(closure))
        };

        // `new.target`: an ordinary call clears it, a construct installs the pending target; an arrow
        // inherits the enclosing value. Saved and restored around the body.
        let saved_new_target = self.new_target.clone();
        let saved_field_init = self.in_field_init_code;
        let saved_agb = self.in_async_gen_body;
        if !func.is_arrow {
            self.new_target = if is_construct {
                std::mem::replace(&mut self.pending_new_target, Value::Undefined)
            } else {
                Value::Undefined
            };
            // An ordinary function body is not class-field-initializer code, nor an async
            // generator's own body (an arrow inherits both).
            self.in_field_init_code = false;
            self.in_async_gen_body = false;
        }

        if !func.is_arrow {
            // `this` binding. Strict: pass through. Sloppy: undefined/null → global; primitive → box.
            let this_val = if func.is_strict || is_construct {
                this
            } else {
                match this {
                    Value::Undefined | Value::Null => Value::Obj(self.global.clone()),
                    other @ Value::Obj(_) => other,
                    // Sloppy mode: a primitive `this` is boxed to its wrapper object (ToObject).
                    prim => crate::builtins::box_primitive_pub(self, prim),
                }
            };
            // `new.target` resolves lexically (arrows and closures created here keep seeing this
            // function's value, even after it returns).
            scope.borrow_mut().vars.insert(
                "%newtarget%".to_string(),
                Binding {
                    value: self.new_target.clone(),
                    mutable: false,
                    strict_immutable: false,
                    initialized: true,
                    import_ref: None,
                    deletable: false,
                },
            );
            // A derived class constructor's `this` stays in TDZ until `super()` runs.
            let derived_tdz = is_construct
                && self
                    .class_info
                    .get(&(Rc::as_ptr(fn_obj) as usize))
                    .map(|ci| ci.derived)
                    .unwrap_or(false);
            scope.borrow_mut().vars.insert(
                "this".to_string(),
                Binding {
                    value: this_val,
                    mutable: false,
                    strict_immutable: true,
                    initialized: !derived_tdz,
                    import_ref: None,
                    deletable: false,
                },
            );
            // The `arguments` exotic object: indexed own props + configurable `length`, with
            // `@@iterator` = Array.prototype.values. An unmapped one (strict function OR
            // non-simple parameter list) exposes `callee` as the %ThrowTypeError% poison
            // accessor; a mapped one aliases still-mapped indices to the parameter bindings
            // (see `mapped_arguments`) and carries the function as `callee`.
            let ao = Object::new(Some(self.object_proto.clone()));
            ao.borrow_mut().exotic = crate::value::Exotic::Arguments;
            for (idx, v) in args.iter().enumerate() {
                ao.borrow_mut().props.insert(
                    idx.to_string().as_str(),
                    Property::data(v.clone(), true, true, true),
                );
            }
            ao.borrow_mut().props.insert(
                "length",
                Property::data(Value::Num(args.len() as f64), true, false, true),
            );
            if let Some(sym) = self.iterator_sym.clone() {
                let values = self
                    .array_proto
                    .borrow()
                    .props
                    .get("values")
                    .map(|p| p.value.clone());
                if let Some(v) = values {
                    ao.borrow_mut()
                        .props
                        .insert(Self::sym_key(&sym), Property::builtin(v));
                }
            }
            let simple_params = func
                .params
                .iter()
                .all(|p| !p.rest && p.default.is_none() && matches!(p.pattern, Pattern::Ident(_)));
            if func.is_strict || !simple_params {
                if let Some(tte) = self.extra_protos.get("%ThrowTypeError%").cloned() {
                    ao.borrow_mut().props.insert(
                        "callee",
                        crate::value::Property {
                            value: Value::Undefined,
                            get: Some(Value::Obj(tte.clone())),
                            set: Some(Value::Obj(tte)),
                            accessor: true,
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    );
                }
            } else {
                ao.borrow_mut().props.insert(
                    "callee",
                    crate::value::Property::data(Value::Obj(fn_obj.clone()), true, false, true),
                );
                // Mapped: indices alias the parameter bindings (which live in `scope`). With
                // duplicate parameter names only the LAST occurrence is mapped — earlier indices
                // stay plain data properties holding the original argument values.
                let mut names: Vec<Option<String>> = func
                    .params
                    .iter()
                    .take(args.len())
                    .map(|p| match &p.pattern {
                        Pattern::Ident(n) => Some(n.clone()),
                        _ => None,
                    })
                    .collect();
                let mut seen: Vec<String> = Vec::new();
                for slot in names.iter_mut().rev() {
                    if let Some(n) = slot {
                        if seen.contains(n) {
                            *slot = None;
                        } else {
                            seen.push(n.clone());
                        }
                    }
                }
                if names.iter().any(Option::is_some) {
                    self.gc_pin(&ao);
                    self.mapped_arguments
                        .insert(Rc::as_ptr(&ao) as usize, (scope.clone(), names));
                }
            }
            scope.borrow_mut().vars.insert(
                "arguments".to_string(),
                Binding {
                    value: Value::Obj(ao),
                    mutable: true,
                    strict_immutable: false,
                    initialized: true,
                    import_ref: None,
                    deletable: false,
                },
            );
            // (A named function expression's self-name binds in its own environment, created
            // before the variable environment above.)
        }

        // Parameter binding may throw (a default initializer, a destructuring mismatch, or an
        // EvalDeclarationInstantiation conflict). For an ordinary async function this abrupt
        // completion rejects the returned promise rather than throwing synchronously; every other
        // kind of function (including async *generators*) throws synchronously.
        let bind_result = self.bind_params(&func.params, args, &scope);

        // With parameter expressions, the body runs in a fresh variable environment below the
        // parameter scope; a `var` sharing a parameter's name starts with that parameter's value.
        let body = if has_param_exprs {
            new_var_scope(Some(scope.clone()))
        } else {
            scope.clone()
        };
        let param_seed = if has_param_exprs {
            Some(scope.clone())
        } else {
            None
        };

        let saved_strict = self.strict;
        self.strict = func.is_strict;

        if func.is_async && !func.is_generator {
            if let Err(e) = bind_result {
                self.strict = saved_strict;
                self.new_target = saved_new_target;
                self.in_field_init_code = saved_field_init;
                self.in_async_gen_body = saved_agb;
                let reason = abrupt_value(e);
                let promise = self.new_promise();
                self.reject_promise(&promise, reason);
                return Ok(promise);
            }
            let r = self.run_async(func, &body, param_seed);
            self.strict = saved_strict;
            self.new_target = saved_new_target;
            self.in_field_init_code = saved_field_init;
            self.in_async_gen_body = saved_agb;
            return r;
        }

        if let Err(e) = bind_result {
            self.strict = saved_strict;
            self.new_target = saved_new_target;
            self.in_field_init_code = saved_field_init;
            self.in_async_gen_body = saved_agb;
            return Err(e);
        }

        // Generators (sync and async) suspend at each yield on their own coroutine; see run_generator.
        if func.is_generator {
            // The generator object's [[Prototype]] comes from the function's own `.prototype`.
            let gen_proto = fn_obj
                .borrow()
                .props
                .get("prototype")
                .map(|p| p.value.clone());
            let gen = self.run_generator(func, &body, param_seed, gen_proto);
            self.strict = saved_strict;
            self.new_target = saved_new_target;
            self.in_field_init_code = saved_field_init;
            self.in_async_gen_body = saved_agb;
            return gen;
        }

        // Hoist `var`/function declarations into the function scope before executing the body.
        // ("arguments" joins the blocked names: an Annex B block function of that name would
        // clash with the arguments object, so it is not var-hoisted.)
        let mut param_names = param_bound_names(&func.params);
        if !func.is_arrow {
            param_names.push("arguments".to_string());
        }
        self.hoist(&func.body, &body, &param_names);
        if let Some(ps) = &param_seed {
            self.seed_param_vars(ps, &body);
        }
        // Pre-declare body-level `let`/`const` in their temporal dead zone.
        self.declare_block_lexicals(&func.body, &body, false);

        // The function body is a disposal boundary for its `using` declarations.
        let has_using = func.body.iter().any(crate::eval::stmt_declares_using);
        if has_using {
            self.using_stack.push(Vec::new());
        }
        // `return f(...)` is a proper tail call only in a strict, ordinary, non-constructor body.
        let saved_tco = std::mem::replace(
            &mut self.tco_ok,
            func.is_strict && !func.is_generator && !func.is_async && !is_construct,
        );
        let mut result = Ok(Value::Undefined);
        for stmt in &func.body {
            match self.exec_stmt(stmt, &body) {
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
        self.tco_ok = saved_tco;
        if has_using {
            let frame = self.using_stack.pop().unwrap_or_default();
            result = self.dispose_frame(frame, result);
        }
        // [[Construct]]: a non-object return yields the *current* `this` binding — which a
        // derived constructor's super() may have rebound to a base constructor's returned object.
        // A derived constructor may only return an object or undefined (TypeError otherwise),
        // and its `this` must have been initialized by super() (ReferenceError otherwise); both
        // errors are created in the caller's realm (the callee context has conceptually popped).
        if is_construct && !func.is_arrow {
            let derived = self
                .class_info
                .get(&(Rc::as_ptr(fn_obj) as usize))
                .map(|c| c.derived)
                .unwrap_or(false);
            if let Ok(v) = &result {
                if !matches!(v, Value::Obj(_)) {
                    if derived && !matches!(v, Value::Undefined | Value::Empty) {
                        result = Err(self.throw_in_caller_realm(
                            "TypeError",
                            "a derived constructor may only return an object or undefined",
                        ));
                    } else {
                        let mut cur = Some(body.clone());
                        while let Some(scope) = cur {
                            let found = scope
                                .borrow()
                                .vars
                                .get("this")
                                .map(|b| (b.value.clone(), b.initialized));
                            if let Some((t, init)) = found {
                                result = if derived && !init {
                                    Err(self.throw_in_caller_realm(
                                        "ReferenceError",
                                        "derived constructor returned without calling super()",
                                    ))
                                } else {
                                    Ok(t)
                                };
                                break;
                            }
                            let parent = scope.borrow().parent.clone();
                            cur = parent;
                        }
                    }
                }
            }
        }
        self.strict = saved_strict;
        self.new_target = saved_new_target;
        self.in_field_init_code = saved_field_init;
        self.in_async_gen_body = saved_agb;
        result
    }

    /// Start a generator: spawn its coroutine (parked until the first `next`) and return the
    /// generator object. The body runs lazily on its own thread, suspending at each `yield`.
    fn run_generator(
        &mut self,
        func: &Rc<Function>,
        scope: &Env,
        param_seed: Option<Env>,
        gen_proto: Option<Value>,
    ) -> Result<Value, Abrupt> {
        let func = func.clone();
        let scope = scope.clone();
        let is_async = func.is_async;
        let body: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> = Box::new(move |i| {
            let saved_strict = i.strict;
            i.strict = func.is_strict;
            let mut pn = param_bound_names(&func.params);
            if !func.is_arrow {
                pn.push("arguments".to_string());
            }
            i.hoist(&func.body, &scope, &pn);
            if let Some(ps) = &param_seed {
                i.seed_param_vars(ps, &scope);
            }
            i.declare_block_lexicals(&func.body, &scope, false);
            crate::coroutine::set_async_gen(is_async);
            let saved_agb = std::mem::replace(&mut i.in_async_gen_body, is_async);
            let has_using = func.body.iter().any(crate::eval::stmt_declares_using);
            if has_using {
                i.using_stack.push(Vec::new());
            }
            let mut result: Result<Value, Abrupt> = Ok(Value::Undefined);
            for stmt in &func.body {
                match i.exec_stmt(stmt, &scope) {
                    Ok(_) => {}
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            if has_using {
                let frame = i.using_stack.pop().unwrap_or_default();
                result = i.dispose_frame(frame, result);
            }
            let outcome = match result {
                Ok(_) => crate::coroutine::Suspend::Done(Value::Undefined),
                Err(Abrupt::Return(v)) => crate::coroutine::Suspend::Done(v),
                Err(Abrupt::Throw(e)) => crate::coroutine::Suspend::Throw(e),
                Err(_) => crate::coroutine::Suspend::Done(Value::Undefined),
            };
            i.strict = saved_strict;
            outcome
        });
        let ptr = self as *mut Interp;
        let coro = crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(body));
        let obj = self.make_generator(is_async, gen_proto);
        if let Value::Obj(o) = &obj {
            self.gc_pin(o);
            self.generators.insert(Rc::as_ptr(o) as usize, coro);
            if is_async {
                self.async_gens.insert(Rc::as_ptr(o) as usize);
            }
        }
        Ok(obj)
    }

    /// Start an async function: spawn its coroutine, return a promise that settles when the body
    /// finishes. Each `await` parks the coroutine; a microtask resumes it once the awaited value
    /// settles.
    fn run_async(
        &mut self,
        func: &Rc<Function>,
        scope: &Env,
        param_seed: Option<Env>,
    ) -> Result<Value, Abrupt> {
        let func = func.clone();
        let scope = scope.clone();
        let body: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> = Box::new(move |i| {
            let saved_strict = i.strict;
            i.strict = func.is_strict;
            let mut pn = param_bound_names(&func.params);
            if !func.is_arrow {
                pn.push("arguments".to_string());
            }
            i.hoist(&func.body, &scope, &pn);
            if let Some(ps) = &param_seed {
                i.seed_param_vars(ps, &scope);
            }
            i.declare_block_lexicals(&func.body, &scope, false);
            let has_using = func.body.iter().any(crate::eval::stmt_declares_using);
            if has_using {
                i.using_stack.push(Vec::new());
            }
            let mut result: Result<Value, Abrupt> = Ok(Value::Undefined);
            for stmt in &func.body {
                match i.exec_stmt(stmt, &scope) {
                    Ok(_) => {}
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            if has_using {
                let frame = i.using_stack.pop().unwrap_or_default();
                result = i.dispose_frame(frame, result);
            }
            let outcome = match result {
                Ok(_) => crate::coroutine::Suspend::Done(Value::Undefined),
                Err(Abrupt::Return(v)) => crate::coroutine::Suspend::Done(v),
                Err(Abrupt::Throw(e)) => crate::coroutine::Suspend::Throw(e),
                Err(_) => crate::coroutine::Suspend::Done(Value::Undefined),
            };
            i.strict = saved_strict;
            outcome
        });
        let ptr = self as *mut Interp;
        let coro = crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(body));
        let promise = self.new_promise();
        if let Value::Obj(o) = &promise {
            self.gc_pin(o);
            self.generators.insert(Rc::as_ptr(o) as usize, coro);
        }
        let key = match &promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => unreachable!(),
        };
        self.drive_async(
            key,
            promise.clone(),
            crate::coroutine::Resume::Next(Value::Undefined),
        );
        Ok(promise)
    }

    /// Resume an async coroutine and react to how it parks: an `await` (Yield) attaches a microtask
    /// that re-drives it once the awaited value settles; completion settles the result promise.
    pub(crate) fn drive_async(
        &mut self,
        key: usize,
        promise: Value,
        signal: crate::coroutine::Resume,
    ) {
        use crate::coroutine::Suspend;
        let mut coro = match self.generators.remove(&key) {
            Some(c) => c,
            None => return,
        };
        let suspend = coro.resume(self, signal);
        match suspend {
            Suspend::Await(awaited) => {
                self.generators.insert(key, coro); // still running
                                                   // Await → PromiseResolve(%Promise%, value); its abrupt (a poisoned `constructor`
                                                   // getter) throws at the `await` itself.
                let px = match self.promise_resolve_checked(awaited) {
                    Ok(p) => p,
                    Err(e) => {
                        return self.drive_async(key, promise, crate::coroutine::Resume::Throw(e))
                    }
                };
                let on_f = self.make_async_reaction(&promise, true);
                let on_r = self.make_async_reaction(&promise, false);
                self.promise_then(&px, on_f, on_r);
            }
            Suspend::Yield(v) => self.resolve_promise(&promise, v),
            Suspend::Done(v) => self.resolve_promise(&promise, v),
            Suspend::Throw(e) => self.reject_promise(&promise, e),
        }
    }

    /// Drive an async generator's coroutine, settling the `next()`/`return()`/`throw()` result
    /// promise `r` with `{value, done}`. An `await` parks the generator (the promise stays pending
    /// until a later `yield`/return); a `yield` fulfils the promise.
    pub(crate) fn drive_async_gen(
        &mut self,
        key: usize,
        r: Value,
        signal: crate::coroutine::Resume,
    ) {
        // A request while a step is in progress (running or awaiting) queues behind it.
        if self.async_gen_busy.contains(&key) {
            self.async_gen_queue
                .entry(key)
                .or_default()
                .push_back((r, signal));
            return;
        }
        self.async_gen_busy.insert(key);
        self.drive_async_gen_inner(key, r, signal);
    }

    /// The step just completed (resolved or rejected its promise): pump the next queued request,
    /// or clear the busy flag.
    pub(crate) fn finish_async_gen_step(&mut self, key: usize) {
        match self
            .async_gen_queue
            .get_mut(&key)
            .and_then(|q| q.pop_front())
        {
            Some((r, signal)) => self.drive_async_gen_inner(key, r, signal),
            None => {
                self.async_gen_busy.remove(&key);
            }
        }
    }

    pub(crate) fn drive_async_gen_inner(
        &mut self,
        key: usize,
        r: Value,
        signal: crate::coroutine::Resume,
    ) {
        use crate::coroutine::{Resume, Suspend};
        let mut coro = match self.generators.remove(&key) {
            Some(c) => c,
            None => {
                let res = self.iter_result_obj(Value::Undefined, true);
                self.resolve_promise(&r, res);
                return;
            }
        };
        if coro.done {
            self.generators.insert(key, coro);
            match signal {
                Resume::Throw(e) => {
                    self.reject_promise(&r, e);
                    self.finish_async_gen_step(key);
                }
                // AsyncGeneratorAwaitReturn: the returned value is awaited before the request
                // promise settles with { value, done: true }.
                Resume::Return(v) => self.async_gen_await_return(key, r, v),
                Resume::Next(_) => {
                    let res = self.iter_result_obj(Value::Undefined, true);
                    self.resolve_promise(&r, res);
                    self.finish_async_gen_step(key);
                }
            }
            return;
        }
        // suspendedStart: a return() before the first next() awaits its value without ever
        // running the body.
        if !coro.started {
            if let Resume::Return(v) = signal {
                self.generators.insert(key, coro);
                return self.async_gen_await_return(key, r, v);
            }
        }
        let suspend = coro.resume(self, signal);
        self.generators.insert(key, coro);
        match suspend {
            Suspend::Yield(v) => {
                let res = self.iter_result_obj(v, false);
                self.resolve_promise(&r, res);
                self.finish_async_gen_step(key);
            }
            Suspend::Await(x) => {
                // Still mid-step: the reaction resumes via drive_async_gen_inner, keeping the
                // request queue intact.
                let px = match self.promise_resolve_checked(x) {
                    Ok(p) => p,
                    Err(e) => return self.drive_async_gen_inner(key, r, Resume::Throw(e)),
                };
                let on_f = self.make_async_gen_reaction(key, &r, true);
                let on_r = self.make_async_gen_reaction(key, &r, false);
                self.promise_then(&px, on_f, on_r);
            }
            Suspend::Done(v) => {
                let res = self.iter_result_obj(v, true);
                self.resolve_promise(&r, res);
                self.finish_async_gen_step(key);
            }
            Suspend::Throw(e) => {
                self.reject_promise(&r, e);
                self.finish_async_gen_step(key);
            }
        }
    }

    /// AsyncGeneratorAwaitReturn: await the `return()` value; fulfilment completes the generator
    /// and settles the request with `{ value: awaited, done: true }`, rejection rejects it.
    fn async_gen_await_return(&mut self, key: usize, r: Value, v: Value) {
        let px = match self.promise_resolve_checked(v) {
            Ok(p) => p,
            Err(e) => {
                self.reject_promise(&r, e);
                self.finish_async_gen_step(key);
                return;
            }
        };
        let on_f = self.make_async_gen_return_reaction(key, &r, true);
        let on_r = self.make_async_gen_return_reaction(key, &r, false);
        self.promise_then(&px, on_f, on_r);
    }

    fn make_async_gen_return_reaction(&mut self, key: usize, r: &Value, fulfil: bool) -> Value {
        let target = self.make_native(
            "",
            1,
            if fulfil {
                crate::builtins::async_gen_return_fulfil
            } else {
                crate::builtins::async_gen_return_reject
            },
        );
        let bound = Object::new(Some(self.function_proto.clone()));
        bound.borrow_mut().call = Callable::Bound {
            target,
            this: r.clone(),
            args: vec![Value::Num(key as f64), r.clone()],
        };
        Value::Obj(bound)
    }

    /// A `{ value, done }` iterator-result object.
    pub(crate) fn iter_result_obj(&mut self, value: Value, done: bool) -> Value {
        let o = self.new_object();
        {
            let mut b = o.borrow_mut();
            b.props
                .insert("value", Property::data(value, true, true, true));
            b.props
                .insert("done", Property::data(Value::Bool(done), true, true, true));
        }
        Value::Obj(o)
    }

    fn make_async_gen_reaction(&mut self, key: usize, r: &Value, fulfil: bool) -> Value {
        let target = self.make_native(
            "",
            1,
            if fulfil {
                crate::builtins::async_gen_react_fulfil
            } else {
                crate::builtins::async_gen_react_reject
            },
        );
        let bound = Object::new(Some(self.function_proto.clone()));
        // `args` carries the generator key (as a number marker) and the result promise.
        bound.borrow_mut().call = Callable::Bound {
            target,
            this: r.clone(),
            args: vec![Value::Num(key as f64), r.clone()],
        };
        Value::Obj(bound)
    }

    /// PromiseResolve with the observable `Get(x, "constructor")` on a native promise — a
    /// poisoned getter surfaces as the returned error.
    pub(crate) fn promise_resolve_checked(&mut self, v: Value) -> Result<Value, Value> {
        if let Value::Obj(o) = &v {
            if self.promises.contains_key(&(Rc::as_ptr(o) as usize)) {
                return match self.get_member(&v, "constructor") {
                    Ok(_) => Ok(v),
                    Err(Abrupt::Throw(e)) => Err(e),
                    Err(_) => Err(Value::Undefined),
                };
            }
        }
        let p = self.new_promise();
        self.resolve_promise(&p, v);
        Ok(p)
    }

    /// A bound reaction that re-drives the async coroutine when the awaited promise settles.
    fn make_async_reaction(&mut self, promise: &Value, fulfil: bool) -> Value {
        let target = self.make_native(
            "",
            1,
            if fulfil {
                crate::builtins::async_react_fulfil
            } else {
                crate::builtins::async_react_reject
            },
        );
        let bound = Object::new(Some(self.function_proto.clone()));
        bound.borrow_mut().call = Callable::Bound {
            target,
            this: promise.clone(),
            args: vec![promise.clone()],
        };
        Value::Obj(bound)
    }

    fn bind_params(&mut self, params: &[Param], args: &[Value], scope: &Env) -> Result<(), Abrupt> {
        // With parameter expressions, every parameter's binding is created (uninitialized) up front,
        // so a default initializer naming a later parameter is a temporal-dead-zone reference and a
        // direct `eval` observes the parameter bindings (matching FunctionDeclarationInstantiation).
        if params_have_expr(params) {
            let mut names = Vec::new();
            for p in params {
                pattern_idents(&p.pattern, &mut names);
            }
            for name in names {
                scope
                    .borrow_mut()
                    .vars
                    .insert(name, Binding::data(Value::Undefined, true, false));
            }
        }
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

    /// When a function has parameter expressions, a body `var` sharing a parameter's name starts
    /// with that parameter's value (FunctionDeclarationInstantiation step for the separate variable
    /// environment). `hoist` has just created the body's `var` bindings as `undefined`; fill in the
    /// initial value from the parameter scope for names it declares.
    fn seed_param_vars(&self, param_scope: &Env, body_scope: &Env) {
        let names: Vec<String> = body_scope.borrow().vars.keys().cloned().collect();
        for name in names {
            if name == "this" {
                continue;
            }
            let seeded = param_scope
                .borrow()
                .vars
                .get(&name)
                .filter(|b| b.initialized)
                .map(|b| b.value.clone());
            if let Some(v) = seeded {
                let mut bs = body_scope.borrow_mut();
                if let Some(b) = bs.vars.get_mut(&name) {
                    // Only a plain `var` slot (still `undefined`) inherits the parameter value; a
                    // function declaration keeps its function object.
                    if matches!(b.value, Value::Undefined) {
                        b.value = v;
                    }
                }
            }
        }
    }

    pub fn construct(&mut self, callee: Value, args: &[Value]) -> Result<Value, Abrupt> {
        let nt = callee.clone();
        self.construct_nt(callee, args, nt)
    }

    /// Like `construct`, but with an explicit `new.target` (for `Reflect.construct`'s third argument
    /// and a proxy's `[[Construct]]` forwarding, where new.target differs from the callee).
    pub fn construct_nt(
        &mut self,
        callee: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Abrupt> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        let r = self.construct_inner(callee, args, new_target);
        self.depth -= 1;
        r
    }

    fn construct_inner(
        &mut self,
        callee: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Abrupt> {
        // IsConstructor is checked in the *caller's* realm, before any realm swap — the
        // TypeError for `new nonCtor` belongs to the code doing the `new`.
        if !matches!(&callee, Value::Obj(_)) || !self.value_is_constructor(&callee) {
            return Err(self.throw(
                "TypeError",
                format!("{} is not a constructor", type_name(&callee)),
            ));
        }
        // A cross-realm constructor runs with its own realm's intrinsics active. The caller's
        // realm is remembered for the [[Construct]] errors thrown after the callee context pops.
        if !self.realms.is_empty() {
            if let Value::Obj(o) = &callee {
                if !self.proxies.contains_key(&(Rc::as_ptr(o) as usize)) {
                    if let Some(gptr) = self.callee_realm_global(o) {
                        let saved = self.snapshot_realm();
                        let target = self.realms[&gptr].snapshot_clone();
                        self.restore_realm(&target);
                        let saved_ccr = self.ctor_caller_realm.replace(saved.snapshot_clone());
                        let r = self.construct_dispatch(callee, args, new_target);
                        self.ctor_caller_realm = saved_ccr;
                        self.restore_realm(&saved);
                        return r;
                    }
                }
            }
            let saved_ccr = self.ctor_caller_realm.take();
            let r = self.construct_dispatch(callee, args, new_target);
            self.ctor_caller_realm = saved_ccr;
            return r;
        }
        self.construct_dispatch(callee, args, new_target)
    }

    fn construct_dispatch(
        &mut self,
        callee: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Abrupt> {
        let obj = match &callee {
            Value::Obj(o) => o.clone(),
            _ => return Err(self.throw("TypeError", "value is not a constructor")),
        };
        // Proxy with a `construct` trap (or forward to the target).
        if !self.proxies.is_empty() {
            if let Some((target, handler)) = self.proxies.get(&(Rc::as_ptr(&obj) as usize)).cloned()
            {
                let trap = self.get_member(&handler, "construct")?;
                if matches!(trap, Value::Undefined | Value::Null) {
                    // Forward to the target's [[Construct]] with the original new.target.
                    return self.construct_inner(target, args, new_target);
                }
                if !trap.is_callable() {
                    return Err(self.throw("TypeError", "proxy 'construct' trap is not callable"));
                }
                let arr = self.make_array(args.to_vec());
                let result = self.call(trap, handler, &[target, arr, new_target.clone()])?;
                // [[Construct]] invariant: the trap must return an Object.
                if !matches!(result, Value::Obj(_)) {
                    return Err(
                        self.throw("TypeError", "proxy construct trap returned a non-object")
                    );
                }
                return Ok(result);
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
                // `new_target` is exposed so a native constructor can derive the instance prototype
                // from it (OrdinaryCreateFromConstructor / subclassing / Reflect.construct).
                let saved = self.constructing;
                let saved_nt = self.new_target.clone();
                self.constructing = true;
                self.new_target = new_target;
                let r = f(self, Value::Undefined, args).map_err(Abrupt::Throw);
                self.constructing = saved;
                self.new_target = saved_nt;
                r
            }
            Callable::User(func, env) => {
                // Arrows, concise methods, getters/setters, generators and async functions have no
                // [[Construct]].
                if func.is_arrow || func.is_method || func.is_generator || func.is_async {
                    return Err(self.throw("TypeError", "this function is not a constructor"));
                }
                // OrdinaryCreateFromConstructor: the new instance's prototype comes from
                // new.target; a non-object `prototype` falls back to new.target's realm's
                // %Object.prototype% (GetFunctionRealm).
                let proto = match self.get_member(&new_target, "prototype")? {
                    Value::Obj(p) => Some(p),
                    _ => {
                        let realm = match &new_target {
                            Value::Obj(nt) => self.get_function_realm_global(nt)?,
                            _ => None,
                        };
                        realm
                            .and_then(|g| self.realms.get(&g).map(|rs| rs.object_proto.clone()))
                            .or_else(|| Some(self.object_proto.clone()))
                    }
                };
                let this = Object::new(proto);
                let this_val = Value::Obj(this);
                self.pending_new_target = new_target.clone();
                // Class constructors run field initializers (and, when derived, defer `this` setup
                // to `super()`); plain function constructors just run their body.
                if self.class_info.contains_key(&(Rc::as_ptr(&obj) as usize)) {
                    let ret = self.run_constructor_on(&callee, &this_val, args)?;
                    // A constructor that explicitly returns an object overrides the instance;
                    // derived-constructor return/`this` validation happens in call_user, which
                    // can see the `this` binding.
                    Ok(match ret {
                        Value::Obj(_) => ret,
                        _ => this_val,
                    })
                } else {
                    let ret = self.call_user(&func, env, this_val.clone(), args, true, &obj)?;
                    Ok(match ret {
                        Value::Obj(_) => ret,
                        _ => this_val,
                    })
                }
            }
            Callable::Bound {
                target,
                args: bargs,
                ..
            } => {
                let mut all = bargs.clone();
                all.extend_from_slice(args);
                // BoundFunction [[Construct]]: if new.target is the bound function itself, use the
                // bound target as new.target instead.
                let nt = if self.strict_equals(&new_target, &callee) {
                    Value::Obj(target.clone())
                } else {
                    new_target
                };
                self.construct_nt(Value::Obj(target), &all, nt)
            }
            Callable::None
            | Callable::WrappedShadow { .. }
            | Callable::WrappedCross { .. }
            | Callable::AccessorGet(_)
            | Callable::AccessorSet(_)
            | Callable::PropGet(_)
            | Callable::PropSet(_) => Err(self.throw("TypeError", "value is not a constructor")),
        }
    }

    // ----- program / statement execution ------------------------------------------------------

    /// Proxy `[[Get]]` invariant: a non-configurable non-writable data property on the target must be
    /// reported with its actual value; a non-configurable accessor with no getter must report
    /// undefined. (`Abrupt` carries the thrown TypeError.)
    fn proxy_get_invariant(
        &mut self,
        target: &Value,
        key: &str,
        result: &Value,
    ) -> Result<(), Abrupt> {
        let prop = match target {
            Value::Obj(t) => t.borrow().props.get(key).cloned(),
            _ => None,
        };
        if let Some(p) = prop {
            if !p.configurable {
                let bad = if p.accessor {
                    matches!(&p.get, None | Some(Value::Undefined))
                        && !matches!(result, Value::Undefined)
                } else {
                    !p.writable && !crate::builtins::same_value_pub(result, &p.value)
                };
                if bad {
                    return Err(self.throw(
                        "TypeError",
                        "proxy 'get' trap violated an invariant for a non-configurable property",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Proxy `[[Set]]` invariant: a `true` result can't contradict a non-configurable non-writable
    /// data property (value must match) or a non-configurable accessor with no setter.
    pub(crate) fn proxy_set_invariant(
        &mut self,
        target: &Value,
        key: &str,
        value: &Value,
    ) -> Result<(), Abrupt> {
        let prop = match target {
            Value::Obj(t) => t.borrow().props.get(key).cloned(),
            _ => None,
        };
        if let Some(p) = prop {
            if !p.configurable {
                let bad = if p.accessor {
                    matches!(&p.set, None | Some(Value::Undefined))
                } else {
                    !p.writable && !crate::builtins::same_value_pub(value, &p.value)
                };
                if bad {
                    return Err(self.throw(
                        "TypeError",
                        "proxy 'set' trap violated an invariant for a non-configurable property",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn run_program(&mut self, body: &[Stmt]) -> Result<Value, Value> {
        // GlobalDeclarationInstantiation early checks, before any binding is created. A probe
        // hoist into a throwaway scope yields this script's VarDeclaredNames.
        let probe = new_scope(None);
        self.hoist(body, &probe, &[]);
        let mut lex_names: Vec<String> = Vec::new();
        for s in body {
            match unwrap_export(s) {
                Stmt::VarDecl {
                    kind:
                        crate::ast::DeclKind::Let
                        | crate::ast::DeclKind::Const
                        | crate::ast::DeclKind::Using
                        | crate::ast::DeclKind::AwaitUsing,
                    decls,
                } => {
                    for (pat, _) in decls {
                        pattern_idents(pat, &mut lex_names);
                    }
                }
                Stmt::ClassDecl(c) => lex_names.extend(c.name.clone()),
                _ => {}
            }
        }
        for n in &lex_names {
            // HasVarDeclaration / HasLexicalDeclaration / HasRestrictedGlobalProperty.
            let restricted = self
                .global
                .borrow()
                .props
                .get(n.as_str())
                .is_some_and(|p| !p.configurable);
            if self.global_env.borrow().vars.contains_key(n)
                || self.global_var_names.contains(n)
                || restricted
            {
                return Err(self.make_error(
                    "SyntaxError",
                    format!("Identifier '{n}' has already been declared"),
                ));
            }
        }
        let extensible = self.global.borrow().extensible;
        for (name, binding) in probe.borrow().vars.iter() {
            if self.global_env.borrow().vars.contains_key(name) {
                return Err(self.make_error(
                    "SyntaxError",
                    format!("Identifier '{name}' has already been declared"),
                ));
            }
            let existing = self
                .global
                .borrow()
                .props
                .get(name.as_str())
                .map(|p| (p.configurable, p.accessor, p.writable, p.enumerable));
            if !matches!(binding.value, Value::Undefined) {
                // CanDeclareGlobalFunction.
                let ok = match existing {
                    Some((true, ..)) => true,
                    Some((false, accessor, writable, enumerable)) => {
                        !accessor && writable && enumerable
                    }
                    None => extensible,
                };
                if !ok {
                    return Err(self.make_error(
                        "TypeError",
                        format!("cannot declare global function '{name}'"),
                    ));
                }
            } else if existing.is_none() && !extensible {
                // CanDeclareGlobalVar.
                return Err(self.make_error(
                    "TypeError",
                    format!("cannot declare global variable '{name}'"),
                ));
            }
        }
        let new_vars: Vec<String> = probe.borrow().vars.keys().cloned().collect();
        self.global_var_names.extend(new_vars.iter().cloned());
        self.hoist(body, &self.global_env.clone(), &[]);
        // The global Environment Record is object-backed: this script's `var`/`function` bindings
        // (which `hoist` just placed in `global_env`) become properties of the global object, so
        // they are visible as `globalThis.<name>` and writes stay in sync. Only the probe-computed
        // names move — lexical bindings from earlier scripts stay in the environment.
        for name in new_vars {
            let Some(binding) = self.global_env.borrow_mut().vars.remove(&name) else {
                continue;
            };
            let existing = self
                .global
                .borrow()
                .props
                .get(name.as_str())
                .map(|p| (p.writable, p.configurable));
            let is_func = !matches!(binding.value, Value::Undefined);
            match existing {
                None => {
                    self.global.borrow_mut().props.insert(
                        name.as_str(),
                        Property::data(binding.value, true, true, false),
                    );
                }
                // CreateGlobalFunctionBinding: a configurable property is fully redefined; a
                // non-configurable one (already validated writable) just takes the new value.
                Some((_, true)) if is_func => {
                    self.global.borrow_mut().props.insert(
                        name.as_str(),
                        Property::data(binding.value, true, true, false),
                    );
                }
                Some((true, _)) if is_func => {
                    if let Some(p) = self.global.borrow_mut().props.get_mut(&name) {
                        p.value = binding.value;
                    }
                }
                _ => {}
            }
        }
        // Top-level `let`/`const` are pre-declared in their temporal dead zone.
        self.declare_block_lexicals(body, &self.global_env.clone(), false);
        let env = self.global_env.clone();
        let mut last = Value::Undefined;
        for stmt in body {
            match self.exec_stmt(stmt, &env) {
                Ok(v) => {
                    if !matches!(v, Value::Empty) {
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
    /// at block entry instead (see [`Self::exec_block`]). `param_blocked` carries the formal
    /// parameter names for *function code* — per B.3.3.1 they block the Annex B var-promotion of
    /// same-named block functions (eval and global code pass none; B.3.3.2/3 have no such clause).
    pub(crate) fn hoist(&mut self, stmts: &[Stmt], scope: &Env, param_blocked: &[String]) {
        for stmt in stmts {
            self.hoist_stmt(stmt, scope);
        }
        // Function declarations are also initialised eagerly (in source order, after var names). An
        // anonymous `export default function(){}` is a HoistableDeclaration too, bound to `*default*`.
        for stmt in stmts {
            if let Stmt::FuncDecl(func) = unwrap_export(stmt) {
                let name = match &func.name {
                    Some(n) => n.clone(),
                    None if matches!(stmt, Stmt::ExportDefault(_)) => "*default*".to_string(),
                    None => continue,
                };
                let f = self.make_function(func.clone(), scope.clone());
                if name == "*default*" {
                    self.set_fn_name(&f, "default");
                }
                scope.borrow_mut().vars.insert(
                    name,
                    Binding {
                        value: f,
                        mutable: true,
                        strict_immutable: false,
                        initialized: true,
                        import_ref: None,
                        deletable: false,
                    },
                );
            }
        }
        // Annex B.3.3: in sloppy mode, a function declared inside a block (or an if/label
        // substatement position) also gets a `var`-style binding in the enclosing function/global
        // scope — initialized to undefined, synced with the block binding when the declaration is
        // evaluated — unless a lexical declaration between here and the block makes the equivalent
        // `var` an early error.
        if !self.strict {
            let mut blocked: Vec<String> = block_lexical_names(stmts);
            blocked.extend(param_blocked.iter().cloned());
            for stmt in stmts {
                self.hoist_block_funcs(stmt, scope, false, &mut blocked);
            }
        }
    }

    fn hoist_block_funcs(
        &mut self,
        stmt: &Stmt,
        scope: &Env,
        in_block: bool,
        blocked: &mut Vec<String>,
    ) {
        match stmt {
            Stmt::FuncDecl(func) if in_block => {
                // Annex B.3.3 promotes only *plain* function declarations; a block-scoped
                // generator or async function stays lexical.
                if func.is_generator || func.is_async {
                    return;
                }
                if let Some(name) = &func.name {
                    // Skip entirely if an intervening lexical declaration with the same name
                    // would make the equivalent `var` an early error.
                    if blocked.iter().any(|b| b == name) {
                        return;
                    }
                    // The var binding starts undefined; a pre-existing binding (parameter, an
                    // outer function declaration, a var) is left untouched. Either way the
                    // declaration's evaluation syncs the block value into it.
                    if !scope.borrow().vars.contains_key(name.as_str()) {
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding {
                                value: Value::Undefined,
                                mutable: true,
                                strict_immutable: false,
                                initialized: true,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                    self.annexb_fn_sync
                        .insert(Rc::as_ptr(func) as usize, func.clone());
                }
            }
            Stmt::Block(body) => {
                let added = block_lexical_names(body);
                let pushed = push_blocked(blocked, added);
                // The block's DIRECT function declarations hoist first (their own/sibling names
                // don't block them)...
                for s in body {
                    if matches!(s, Stmt::FuncDecl(_)) {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                }
                // ...then nested statements see those names as lexical (a same-named function in
                // an inner block would clash when var-hoisted through this one).
                let fn_names: Vec<String> = body
                    .iter()
                    .filter_map(|s| match s {
                        Stmt::FuncDecl(f) => f.name.clone(),
                        _ => None,
                    })
                    .collect();
                let pushed_fns = push_blocked(blocked, fn_names);
                for s in body {
                    if !matches!(s, Stmt::FuncDecl(_)) {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                }
                blocked.truncate(blocked.len() - pushed_fns);
                blocked.truncate(blocked.len() - pushed);
            }
            Stmt::If { cons, alt, .. } => {
                self.hoist_block_funcs(cons, scope, true, blocked);
                if let Some(a) = alt {
                    self.hoist_block_funcs(a, scope, true, blocked);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                self.hoist_block_funcs(body, scope, true, blocked)
            }
            // A `for` head's lexical bindings shadow the body: an equivalent `var` inside would
            // be an early error, so same-named block functions there are skipped.
            Stmt::For { init, body, .. } => {
                let mut added = Vec::new();
                if let Some(init) = init {
                    if let ForInit::VarDecl {
                        kind: DeclKind::Let | DeclKind::Const,
                        decls,
                    } = init.as_ref()
                    {
                        for (pat, _) in decls {
                            pattern_idents(pat, &mut added);
                        }
                    }
                }
                let pushed = push_blocked(blocked, added);
                self.hoist_block_funcs(body, scope, true, blocked);
                blocked.truncate(blocked.len() - pushed);
            }
            Stmt::ForInOf {
                decl, left, body, ..
            } => {
                let mut added = Vec::new();
                if matches!(decl, Some(DeclKind::Let | DeclKind::Const)) {
                    pattern_idents(left, &mut added);
                }
                let pushed = push_blocked(blocked, added);
                self.hoist_block_funcs(body, scope, true, blocked);
                blocked.truncate(blocked.len() - pushed);
            }
            Stmt::Labeled { body, .. } | Stmt::With { body, .. } => {
                self.hoist_block_funcs(body, scope, true, blocked)
            }
            Stmt::Switch { cases, .. } => {
                // The switch block's lexicals are shared across all cases.
                let mut added = Vec::new();
                for c in cases {
                    added.extend(block_lexical_names(&c.body));
                }
                let pushed = push_blocked(blocked, added);
                for c in cases {
                    for s in &c.body {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                }
                blocked.truncate(blocked.len() - pushed);
            }
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                {
                    let added = block_lexical_names(block);
                    let pushed = push_blocked(blocked, added);
                    for s in block {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                    blocked.truncate(blocked.len() - pushed);
                }
                if let Some((param, h)) = handler {
                    // A destructuring catch parameter blocks same-named function hoisting out of
                    // the handler; a simple `catch (f)` does not (the B.3.5 legacy var exemption).
                    let mut added = block_lexical_names(h);
                    if let Some(pat) = param {
                        if !matches!(pat, Pattern::Ident(_)) {
                            pattern_idents(pat, &mut added);
                        }
                    }
                    let pushed = push_blocked(blocked, added);
                    for s in h {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                    blocked.truncate(blocked.len() - pushed);
                }
                if let Some(f) = finalizer {
                    let added = block_lexical_names(f);
                    let pushed = push_blocked(blocked, added);
                    for s in f {
                        self.hoist_block_funcs(s, scope, true, blocked);
                    }
                    blocked.truncate(blocked.len() - pushed);
                }
            }
            _ => {}
        }
    }

    fn hoist_stmt(&mut self, stmt: &Stmt, scope: &Env) {
        match stmt {
            Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => self.hoist_stmt(inner, scope),
            Stmt::VarDecl {
                kind: DeclKind::Var,
                decls,
            } => {
                for (pat, _) in decls {
                    let mut names = Vec::new();
                    pattern_idents(pat, &mut names);
                    for name in names {
                        if !scope.borrow().vars.contains_key(&name) {
                            scope.borrow_mut().vars.insert(
                                name,
                                Binding {
                                    value: Value::Undefined,
                                    mutable: true,
                                    strict_immutable: false,
                                    initialized: true,
                                    import_ref: None,
                                    deletable: false,
                                },
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
                    if let ForInit::VarDecl {
                        kind: DeclKind::Var,
                        decls,
                    } = init.as_ref()
                    {
                        for (pat, _) in decls {
                            let mut names = Vec::new();
                            pattern_idents(pat, &mut names);
                            for name in names {
                                scope.borrow_mut().vars.insert(
                                    name,
                                    Binding {
                                        value: Value::Undefined,
                                        mutable: true,
                                        strict_immutable: false,
                                        initialized: true,
                                        import_ref: None,
                                        deletable: false,
                                    },
                                );
                            }
                        }
                    }
                }
                self.hoist_stmt(body, scope);
            }
            Stmt::ForInOf {
                decl: Some(DeclKind::Var),
                left,
                body,
                ..
            } => {
                let mut names = Vec::new();
                pattern_idents(left, &mut names);
                for name in names {
                    scope.borrow_mut().vars.insert(
                        name,
                        Binding {
                            value: Value::Undefined,
                            mutable: true,
                            strict_immutable: false,
                            initialized: true,
                            import_ref: None,
                            deletable: false,
                        },
                    );
                }
                self.hoist_stmt(body, scope);
            }
            Stmt::ForInOf { body, .. } => self.hoist_stmt(body, scope),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
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
        Value::Undefined | Value::Empty => "undefined",
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
