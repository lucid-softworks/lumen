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
    R.get_or_init(|| Mutex::new(Default::default()))
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
    T.get_or_init(|| Mutex::new(Default::default()))
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

/// One entry of the legacy `fn.caller`/`fn.arguments` reflection stack (see `call_user`). The
/// arguments object materializes lazily: a body that never names `arguments` skips building it,
/// and `lazy` keeps what a later reflective read needs to conjure it on demand.
pub struct FnFrame {
    /// `Rc::as_ptr` of the callee. No strong handle is kept: every frame is pushed while its
    /// caller holds the callee alive (the callee `Value` sits on the caller's operand stack or in
    /// the dispatch chain for the whole call — for frames owned by a parked coroutine, the
    /// worker's frozen stack; a torn-down coroutine's worker parks forever rather than unwinding,
    /// which this invariant depends on), so the rare reflective reads reconstruct one via
    /// [`FnFrame::callee`] instead of paying a refcount round-trip on every call.
    pub fn_ptr: usize,
    /// Owning coroutine body (`Interp::cur_coro`; 0 = the main driver): a worker-thread panic
    /// evicts the dead body's frames by this tag (see `ThreadCoro::resume`).
    pub coro: u32,
    pub strict: bool,
    /// The rare per-frame state (a live `arguments` object, or what a reflective `fn.arguments`
    /// read needs to conjure one). Boxed so the common frame stays 24 bytes — frames are pushed
    /// and popped on EVERY call, and the pop's copy-out and drop-check of a fat frame was a
    /// measurable slice of the call path.
    pub extra: Option<Box<FrameExtra>>,
}

/// See [`FnFrame::extra`].
pub struct FrameExtra {
    pub args_obj: Value,
    pub lazy: Option<(Rc<crate::ast::Function>, Rc<[Value]>, Env)>,
}

impl Default for FrameExtra {
    fn default() -> FrameExtra {
        FrameExtra {
            args_obj: Value::Null,
            lazy: None,
        }
    }
}

impl FnFrame {
    /// A strong handle to the callee, reconstructed from `fn_ptr` (see its aliveness invariant).
    pub fn callee(&self) -> Gc {
        let p = self.fn_ptr as *const RefCell<crate::value::Object>;
        unsafe {
            Rc::increment_strong_count(p);
            Rc::from_raw(p)
        }
    }
}

/// The JIT fast call's frame-buffer freelist (see `Interp::frame_pool`). A newtype so teardown
/// frees the raw buffers (their contents are already dropped whenever a buffer is pooled).
pub(crate) struct FramePool(pub(crate) Vec<std::ptr::NonNull<Value>>);

impl std::ops::Deref for FramePool {
    type Target = Vec<std::ptr::NonNull<Value>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for FramePool {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for FramePool {
    fn drop(&mut self) {
        for p in self.0.drain(..) {
            unsafe {
                drop(Box::from_raw(std::slice::from_raw_parts_mut(
                    p.as_ptr() as *mut std::mem::MaybeUninit<Value>,
                    crate::jit::FRAME_BUF,
                )));
            }
        }
    }
}

/// Cached UTF-16 view of one string, keyed by `Rc<str>` identity (see `Interp::str_units`).
/// JS string semantics are code units but the engine stores UTF-8 `str`; without a cache every
/// `charCodeAt`/`[i]`/`.length` is O(len) (a full `is_ascii` scan or unit walk), which turns a
/// scanner loop — a JS parser reading a megabytes-long source one char at a time — into O(n²).
#[derive(Clone)]
pub(crate) enum StrUnits {
    /// Pure ASCII: unit `i` is byte `i` — index the `str` directly.
    Ascii,
    /// The materialized code units.
    Units(Rc<[u16]>),
}

/// Raw state of the last successful regex match, deferred for the legacy `RegExp.$1` statics.
/// `ctor` is the %RegExp% constructor the statics belong to (the realm active at match time).
pub(crate) struct RegexpLastMatch {
    pub ctor: Gc,
    pub input: crate::lstr::LStr,
    pub text: Rc<crate::regex::ReText>,
    pub caps: Vec<Option<(usize, usize)>>,
    pub ngroups: usize,
}

/// One scope's binding map, wrapping the raw hash map so every *structural* mutation — anything
/// that can move entries or change what a name resolves to (insert, remove, clear) — bumps a
/// generation counter. The bytecode tier's per-site name caches hold a raw `&Binding` pointer
/// plus the generation they resolved it at (see `bytecode::NameIc`): a matching generation
/// proves the map hasn't changed shape since, so the pointer is still valid *and* still the
/// right resolution. In-place binding writes (`get_mut`) intentionally don't bump — they can't
/// move entries, and a cache read-through observes the new value, which is exactly correct.
/// Reads pass through via `Deref`; mutations only exist as the inherent methods below, so a new
/// mutation site can't forget the bump (it won't compile).
#[derive(Default)]
pub struct VarMap {
    map: crate::fasthash::FastMap<String, Binding>,
    generation: std::cell::Cell<u32>,
}

impl std::ops::Deref for VarMap {
    type Target = crate::fasthash::FastMap<String, Binding>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl VarMap {
    /// The structural generation (name-cache validation token).
    #[inline]
    pub(crate) fn generation(&self) -> u32 {
        self.generation.get()
    }
    #[inline]
    fn bump(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
    }
    pub fn insert(&mut self, k: String, v: Binding) -> Option<Binding> {
        self.bump();
        self.map.insert(k, v)
    }
    pub fn remove(&mut self, k: &str) -> Option<Binding> {
        self.bump();
        self.map.remove(k)
    }
    pub fn clear(&mut self) {
        self.bump();
        self.map.clear();
    }
    /// In-place binding write: entries don't move, so the generation stays (see the type docs).
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Binding> {
        self.map.get_mut(k)
    }
    /// Byte offset of the generation counter within a `VarMap` (for the JIT's inline template).
    pub(crate) fn generation_offset() -> usize {
        std::mem::offset_of!(VarMap, generation)
    }
}

pub struct Scope {
    pub vars: VarMap,
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
    pub(crate) fn data(value: Value, mutable: bool, initialized: bool) -> Binding {
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

thread_local! {
    /// Every Scope created on this thread, weakly — the cycle collector's scope snapshot
    /// (mirrors `value::GC_REGISTRY` for objects).
    static SCOPE_REGISTRY: RefCell<Vec<std::rc::Weak<RefCell<Scope>>>> =
        const { RefCell::new(Vec::new()) };
}

fn register_scope(e: &Env) {
    SCOPE_REGISTRY.with(|r| r.borrow_mut().push(Rc::downgrade(e)));
}

/// Registered scope entries (live + not-yet-purged dead weaks) on this thread.
fn scope_registry_len() -> usize {
    SCOPE_REGISTRY.with(|r| r.borrow().len())
}

/// Purge dead weak entries, returning the live count. A dead `Weak` still pins its `RcBox`
/// allocation, so an interpreter that churns through scopes without allocating many objects
/// (which is what arms the main GC) must prune on scope volume too — see `gc_check`.
fn scope_registry_prune() -> usize {
    SCOPE_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.retain(|w| w.strong_count() > 0);
        reg.len()
    })
}

/// The live scopes on this thread (purging dead weak entries as it goes).
fn scope_snapshot() -> Vec<Env> {
    SCOPE_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|w| match w.upgrade() {
            Some(e) => {
                live.push(e);
                true
            }
            None => false,
        });
        live
    })
}

pub fn new_scope(parent: Option<Env>) -> Env {
    let e = Rc::new(RefCell::new(Scope {
        vars: Default::default(),
        parent,
        with_obj: None,
        var_boundary: false,
        catch_param: false,
        lexical_names: Vec::new(),
    }));
    register_scope(&e);
    e
}

/// A *variable* environment: the hoisting target for `var`/function declarations (function body,
/// global, or `eval` scope). See [`Scope::var_boundary`].
pub fn new_var_scope(parent: Option<Env>) -> Env {
    let e = Rc::new(RefCell::new(Scope {
        vars: Default::default(),
        parent,
        with_obj: None,
        var_boundary: true,
        catch_param: false,
        lexical_names: Vec::new(),
    }));
    register_scope(&e);
    e
}

/// A `catch (e)` parameter environment: like a block scope, but flagged so a sloppy direct `eval`'s
/// var-hoisting walk skips it (see [`Scope::catch_param`]).
pub fn new_catch_scope(parent: Env) -> Env {
    let e = Rc::new(RefCell::new(Scope {
        vars: Default::default(),
        parent: Some(parent),
        with_obj: None,
        var_boundary: false,
        catch_param: true,
        lexical_names: Vec::new(),
    }));
    register_scope(&e);
    e
}

/// A `with (obj)` environment: identifier lookups consult `obj` before the enclosing scope.
pub fn new_with_scope(parent: Env, obj: Value) -> Env {
    let e = Rc::new(RefCell::new(Scope {
        vars: Default::default(),
        parent: Some(parent),
        with_obj: Some(obj),
        var_boundary: false,
        catch_param: false,
        lexical_names: Vec::new(),
    }));
    register_scope(&e);
    e
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
        RefCell::new(Default::default());
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

/// One megamorphic-stub-cache entry (see [`Interp::stub_cache`]): the derived resolution for a
/// (receiver shape, site name pointer) pair. `name` is the identity of the *site's* interned
/// name `Rc<str>` data pointer — two sites reading the same property text through different
/// `Rc`s simply use separate entries. The default (`name: 0`) can't match a real probe.
#[derive(Clone, Copy)]
pub(crate) struct StubEntry {
    pub(crate) name: usize,
    pub(crate) st: crate::bytecode::IcState,
}

impl Default for StubEntry {
    fn default() -> StubEntry {
        StubEntry {
            name: 0,
            st: crate::bytecode::IcState {
                recv_shape: 0,
                holder_shape: 0,
                slot: 0,
                depth: crate::bytecode::IC_EMPTY,
                mid_ok: 0,
                mid_shape: 0,
                mid2_shape: 0,
            },
        }
    }
}

/// Stub-cache capacity (power of two). 4096 × 24-byte entries = 96 KiB.
const STUB_CACHE_SIZE: usize = 4096;

/// The table index for a (receiver shape, name pointer) pair. The name pointer's low bits are
/// alignment zeros; shift them off before mixing so they contribute entropy.
#[inline(always)]
fn stub_slot(shape: u32, name: &str) -> usize {
    let n = name.as_ptr() as usize >> 3;
    (shape as usize ^ n ^ (n >> 7)) & (STUB_CACHE_SIZE - 1)
}

/// Runtime-probed byte offsets from an `Interp`'s base address to the fields the asm call
/// thunk touches (see the asm-call-frames arc). `Interp` is not `repr(C)`, so nothing here is
/// compile-time stable — every offset is measured against a live instance, and `valid` fails
/// closed. Vec word offsets (`ptr`/`len`/`cap` order) are probed per instantiation, same as
/// [`crate::value::JitLayout`]'s.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)] // consumed incrementally as the asm thunk lands
pub(crate) struct InterpLayout {
    pub depth: usize,
    pub gc_tick: usize,
    pub cur_coro: usize,
    pub constructing: usize,
    pub new_target: usize,
    pub pending_tail: usize,
    /// `fn_frames` Vec base, plus its ptr/len/cap word offsets within the Vec header.
    pub fn_frames: usize,
    pub fnf_ptr_word: usize,
    pub fnf_len_word: usize,
    pub fnf_cap_word: usize,
    /// `frame_pool` (Vec<NonNull<Value>>) base + its word offsets.
    pub frame_pool: usize,
    pub fp_ptr_word: usize,
    pub fp_len_word: usize,
    pub fp_cap_word: usize,
    pub valid: bool,
}

/// Probe the ptr/len/cap word order of a `Vec<T>` header holding at least one element whose
/// data pointer and length are known. Returns (ptr_word, len_word, cap_word) or None.
fn probe_vec_words<T>(v: &Vec<T>, len: usize, cap: usize) -> Option<(usize, usize, usize)> {
    let data = v.as_ptr() as usize;
    let words: [usize; 3] = unsafe { std::mem::transmute_copy::<Vec<T>, [usize; 3]>(v) };
    let mut ptr_w = None;
    let mut len_w = None;
    let mut cap_w = None;
    for (k, w) in words.iter().enumerate() {
        if *w == data && ptr_w.is_none() {
            ptr_w = Some(k * 8);
        } else if *w == len && len_w.is_none() {
            len_w = Some(k * 8);
        } else if *w == cap && cap_w.is_none() {
            cap_w = Some(k * 8);
        }
    }
    Some((ptr_w?, len_w?, cap_w?))
}

pub(crate) fn interp_layout(i: &mut Interp) -> InterpLayout {
    let base = i as *mut Interp as usize;
    let off = |p: usize| p - base;
    // Distinguishable len/cap for the Vec probes: len 3, cap ≥ 4 and != 3.
    let saved_frames = std::mem::take(&mut i.fn_frames);
    i.fn_frames = Vec::with_capacity(7);
    for k in 0..3 {
        i.fn_frames.push(FnFrame {
            fn_ptr: 0x1000 + k,
            coro: 0,
            strict: false,
            extra: None,
        });
    }
    let fnf = probe_vec_words(&i.fn_frames, 3, i.fn_frames.capacity());
    i.fn_frames = saved_frames;
    let saved_pool = std::mem::replace(&mut i.frame_pool.0, Vec::with_capacity(7));
    for _ in 0..3 {
        i.frame_pool
            .0
            .push(std::ptr::NonNull::new(0x2000 as *mut Value).unwrap());
    }
    let fp = probe_vec_words(&i.frame_pool.0, 3, i.frame_pool.0.capacity());
    i.frame_pool.0.clear();
    i.frame_pool.0 = saved_pool;
    let (Some(fnf), Some(fp)) = (fnf, fp) else {
        return InterpLayout::default(); // valid: false
    };
    InterpLayout {
        depth: off(&i.depth as *const _ as usize),
        gc_tick: off(&i.gc_tick as *const _ as usize),
        cur_coro: off(&i.cur_coro as *const _ as usize),
        constructing: off(&i.constructing as *const _ as usize),
        new_target: off(&i.new_target as *const _ as usize),
        pending_tail: off(&i.pending_tail as *const _ as usize),
        fn_frames: off(&i.fn_frames as *const _ as usize),
        fnf_ptr_word: fnf.0,
        fnf_len_word: fnf.1,
        fnf_cap_word: fnf.2,
        frame_pool: off(&i.frame_pool as *const _ as usize),
        fp_ptr_word: fp.0,
        fp_len_word: fp.1,
        fp_cap_word: fp.2,
        valid: true,
    }
}

pub struct Interp {
    pub(crate) global: Gc,
    pub(crate) global_env: Env,
    pub(crate) object_proto: Gc,
    pub(crate) function_proto: Gc,
    pub(crate) array_proto: Gc,
    pub(crate) string_proto: Gc,
    pub(crate) number_proto: Gc,
    pub(crate) boolean_proto: Gc,
    pub(crate) symbol_proto: Gc,
    pub(crate) error_protos: crate::fasthash::FastMap<&'static str, Gc>,
    /// Monotonic id source + registry for live symbols (so a symbol used as a property key can be
    /// recovered for `Object.getOwnPropertySymbols`). `sym_for` backs the `Symbol.for` registry.
    pub(crate) sym_counter: u64,
    pub(crate) sym_registry: crate::fasthash::FastMap<u64, Rc<SymbolData>>,

    pub(crate) console: Vec<String>,
    /// Current strict-mode flag (pushed/popped around function bodies).
    pub(crate) strict: bool,
    /// Execution tier (env `LUMEN_TIER`, CLI `--tier`, [`Engine::set_tier`]). `Interp` is the
    /// reference tree-walker and the default; the bytecode VM is opt-in.
    pub(crate) tier: crate::bytecode::Tier,
    /// Calls before an eligible function tier-ups to bytecode (env `LUMEN_TIER_THRESHOLD`).
    pub(crate) tier_threshold: u32,
    /// Recycled (slots, operand stack) buffers for bytecode-VM activations, so a hot call tree
    /// doesn't allocate two `Vec`s per call (see `bytecode::run`).
    pub(crate) vm_pool: Vec<(Vec<Value>, Vec<Value>)>,
    /// Megamorphic stub cache: a global open-addressed table keyed by (receiver shape, site name
    /// pointer) holding the last derived [`crate::bytecode::IcState`] for that pair. Probed when
    /// both of a site's ways miss, so a site rotating through more receiver shapes than it has
    /// ways (constraint hierarchies, visitor patterns) still resolves by shape compares instead
    /// of re-walking the chain with hashed key lookups. Entries are validated by the same live
    /// shape probe as the per-site ways, so stale entries are misses, never wrong hits — the
    /// table needs no invalidation.
    pub(crate) stub_cache: Vec<std::cell::Cell<StubEntry>>,
    /// Freelist of fixed-size raw frame buffers ([`crate::jit::FRAME_BUF`] `Value`s each) for the
    /// JIT fast call's slots + operand stack — a pop and pointer math per call instead of `Vec`
    /// bookkeeping. Buffers hold no live values while pooled.
    pub(crate) frame_pool: FramePool,
    /// Per-site pins for the property-*creation* inline caches (see
    /// [`crate::bytecode::IC_CREATE`]), keyed by the cache `Cell`'s address: the receiver's
    /// prototype pointer at fill plus a `Weak` pinning that address against recycling (same
    /// shape does NOT imply same prototype, so identity must be part of the key).
    pub(crate) creation_pins:
        crate::fasthash::FastMap<usize, (usize, std::rc::Weak<RefCell<crate::value::Object>>)>,
    /// Strong pins for every global scope a call cache has ever recorded (see
    /// [`crate::bytecode::CallIc::global_env`]): the cached same-realm proof compares the scope
    /// address raw, so those addresses must never be recycled. Bounded by the number of realms
    /// (which the engine keeps alive anyway).
    pub(crate) global_env_pins: Vec<Env>,
    /// The coroutine body currently executing through this interpreter (0 = the main driver);
    /// stamps `FnFrame::coro` so a dead worker's frames can be evicted precisely.
    pub(crate) cur_coro: u32,
    /// The JIT's helper function table, built once (a stable address the machine code indexes
    /// through x21) instead of re-materialized on every call.
    pub(crate) jit_helpers: [usize; crate::jit::N_HELPERS],
    /// Object-graph byte offsets the JIT's inline property-cache templates bake in (measured once;
    /// see [`crate::value::jit_layout`]). Lazily computed on first JIT compile.
    pub(crate) jit_layout: std::cell::OnceCell<crate::value::JitLayout>,
    /// Runtime-probed `Interp` field offsets for the asm call thunk (see [`InterpLayout`]).
    /// Filled by [`Interp::interp_layout_init`] on first JIT compile, like `jit_layout`.
    pub(crate) interp_layout: std::cell::Cell<InterpLayout>,
    /// Whether the JIT's inline property caches are safe to run: they can't cheaply check the
    /// exotic side tables (proxy / typed-array / module namespace / deferred namespace) from
    /// machine code, so this flag latches *false* the first time any object is registered in one,
    /// and the inline templates then fall through to the checked helper forever after. Monotonic
    /// (never re-enabled), so it can only cost the inline speedup, never correctness.
    pub(crate) inline_ic_safe: std::cell::Cell<bool>,
    /// Recently indexed strings' UTF-16 views, keyed by string identity (the held `Rc` pins the
    /// pointer) — see [`StrUnits`]. Small LRU: the hot case is one big source string being
    /// scanned a unit at a time.
    pub(crate) str_units: Vec<(crate::lstr::LStr, StrUnits)>,
    /// Recently prepared regex subjects keyed by string identity (the held `Rc` pins the
    /// pointer), so repeated exec/replace/split over one subject reuses its element vector.
    pub(crate) re_texts: Vec<(crate::lstr::LStr, bool, Rc<crate::regex::ReText>)>,
    /// The last successful `exec` match, kept raw for the legacy `RegExp.$1`-style statics:
    /// the 14 strings materialize into the constructor's hidden props only when an accessor
    /// actually reads them (see `builtins::flush_regexp_legacy`), not on every match.
    pub(crate) regexp_last: Option<RegexpLastMatch>,
    /// Live interpreter recursion depth (expression eval + calls). Bounded by [`MAX_EVAL_DEPTH`]
    /// so runaway recursion throws a RangeError instead of overflowing the native stack.
    pub(crate) depth: u32,
    /// Per-class metadata (instance fields + whether the class extends another), keyed by the
    /// constructor object's pointer (`Rc::as_ptr(..) as usize`). Lets `construct`/`super` run field
    /// initializers without attaching engine data to the `Object` itself.
    pub(crate) class_info: crate::fasthash::FastMap<usize, ClassInfo>,
    /// The global `eval` function object, so a *direct* eval call (`eval(src)` by that name) can be
    /// distinguished from an indirect one and run in the caller's scope.
    pub(crate) eval_fn: Option<Gc>,
    /// Object pointers of every function tagged `__eval_realm` (each realm's `eval`): the
    /// pre-filter `call_dispatch` checks before the property lookup, so ordinary calls skip it.
    pub(crate) eval_realm_fns: crate::fasthash::FastSet<usize>,
    /// The "no-elements-on-array-protos" protector: `(epoch, clean)` — while `epoch` matches the
    /// live [`crate::value::proto_epoch`], `clean` says the canonical `Array.prototype` →
    /// `Object.prototype` chain has no canonical-index own keys, no exotic hops, and terminates
    /// at `Object.prototype`. The dense `push` fast path needs this proof: an indexed accessor
    /// on the chain must intercept the append (OrdinarySet), so the fast path stands down when
    /// the proof lapses. Every defineProperty / proto swap / structural change to a marked
    /// prototype bumps the epoch, forcing a re-verify.
    pub(crate) elems_protector: std::cell::Cell<(u32, bool)>,
    /// Per-CONSTRUCTOR construct cache (`new F(...)` — keyed by callee identity, since a
    /// constructor's derived state doesn't vary per site): the same raw pointers a [`CallIc`]
    /// caches, validated by the same epoch. The `Weak` pins the callee's address against
    /// recycling (ABA), exactly like `Chunk::call_pins`.
    pub(crate) construct_ics:
        crate::fasthash::FastMap<usize, (crate::bytecode::CallIc, std::rc::Weak<RefCell<crate::value::Object>>)>,
    /// `Symbol.iterator`, cached so the iterator protocol can look up `obj[@@iterator]` cheaply.
    pub(crate) iterator_sym: Option<Rc<SymbolData>>,
    /// Well-known symbols, minted once per Interp — additional realms (`$262.createRealm()`) reuse
    /// them so `@@iterator` etc. have the same identity cross-realm, as the spec requires.
    pub(crate) wk_syms: Vec<(&'static str, Value)>,
    /// Set while a `?.` link in the current optional chain saw a nullish base, so the rest of the
    /// chain short-circuits to `undefined`. Reset at each `OptionalChain` boundary.
    pub(crate) short_circuit: bool,
    /// The `import.meta` object for the module currently executing (None in script code).
    pub(crate) import_meta: Option<Value>,
    /// Default referrer for a bare `import()` in script code (so relative specifiers resolve).
    pub(crate) import_base: String,
    /// Loaded module namespace objects, keyed by canonical specifier (for `import()` + caching).
    pub(crate) modules: std::collections::HashMap<String, Value>,
    /// Full module records (parsed body, environment, resolved export tables, evaluation status),
    /// keyed by canonical specifier. Drives the two-phase Instantiate/Evaluate module linking.
    pub(crate) module_recs: std::collections::HashMap<String, crate::modules::ModuleRec>,
    /// Host module loader: `(specifier, referrer, attr_type)` → `(canonical_key, source)`.
    /// `attr_type` is the import's `with { type: ... }` attribute — an attribute-aware host
    /// returns RAW file contents for `json`/`text`/`bytes` (binary latin-1-decoded, one char per
    /// byte) and the engine synthesizes the wrapper module; attribute-blind hosts ignore it.
    #[allow(clippy::type_complexity)]
    pub(crate) module_loader:
        Option<Rc<dyn Fn(&str, &str, Option<&str>) -> Option<(String, String)>>>,
    /// Live module-namespace state keyed by the namespace object's pointer: for each exported name,
    /// how to read its current value (a live binding in some module scope, or a static value for a
    /// star-as namespace re-export). Namespace property reads consult this so they stay live.
    pub(crate) module_ns: crate::fasthash::FastMap<
        usize,
        crate::fasthash::FastMap<String, crate::modules::NsBinding>,
    >,
    /// Backing store for Map/Set/WeakMap/WeakSet instances (ordered entries), keyed by the object's
    /// pointer — the engine analogue of an internal `[[MapData]]` slot.
    pub(crate) map_data: crate::fasthash::FastMap<usize, Vec<(Value, Value)>>,
    /// Prototypes for builtins created after `new()` (Map/Set/Date/...), looked up by name so their
    /// native constructors can stamp the right `[[Prototype]]`.
    pub(crate) extra_protos: crate::fasthash::FastMap<&'static str, Gc>,
    /// ArrayBuffer byte storage, keyed by the ArrayBuffer object's pointer.
    pub(crate) array_buffers: crate::fasthash::FastMap<usize, Vec<u8>>,
    /// SharedArrayBuffer pointers → their global shared-memory id (`array_buffers` keeps a
    /// same-length placeholder so detach/length checks still work; the bytes live in the registry).
    pub(crate) shared_buffers: crate::fasthash::FastMap<usize, u64>,
    /// Immutable ArrayBuffer pointers (created via `transferToImmutable`/`sliceToImmutable`): their
    /// bytes can be read but never written, resized, detached, or transferred.
    pub(crate) immutable_buffers: std::collections::HashSet<usize>,
    /// Whether this agent may block in `Atomics.wait` (false for the main agent, true for the
    /// worker agents spawned by `$262.agent.start`).
    pub(crate) can_block: bool,
    /// Pending `Atomics.waitAsync` operations: each carries the result promise and a channel that a
    /// waiter thread sends "ok"/"timed-out" on. The event loop resolves them as they complete.
    pub(crate) pending_async_waits: Vec<(Value, std::sync::mpsc::Receiver<&'static str>)>,
    /// Host timers from `$262.agent.setTimeout`: (callback, deadline).
    pub(crate) pending_timers: Vec<(Value, std::time::Instant)>,
    /// Agent-harness wiring (present only in spawned agents / a main with agents).
    pub(crate) agent: Option<Box<AgentChannels>>,
    /// TypedArray view state, keyed by the typed-array object's pointer.
    pub(crate) typed_arrays: crate::fasthash::FastMap<usize, TaInfo>,
    /// Object pointers of async generator instances (the AsyncGenerator brand).
    pub(crate) async_gens: std::collections::HashSet<usize>,
    /// The global environment's [[VarNames]]: names declared by `var`/function in global code, for
    /// GlobalDeclarationInstantiation's cross-script clash checks.
    pub(crate) global_var_names: std::collections::HashSet<String>,

    /// GC pins: one `Gc` clone per object that has an entry in a pointer-keyed side table
    /// (typed_arrays, promises, array_buffers, …). The pin keeps the object from being freed by
    /// plain refcounting — which would let a later allocation reuse its address and inherit the
    /// stale side-table entry — so such objects die only in `gc_collect`'s sweep, which evicts
    /// their table entries first. The collector discounts pins when finding roots.
    pub(crate) gc_pins: crate::fasthash::FastMap<usize, Gc>,
    /// The backing ArrayBuffer *object* for each TypedArray (so the `buffer` getter can return it
    /// without storing it as an observable own property). Keyed by the TypedArray's pointer.
    pub(crate) ta_buffer: crate::fasthash::FastMap<usize, Value>,
    /// Each `ShadowRealm` instance owns an isolated realm (a full sub-interpreter), keyed by the
    /// ShadowRealm object's pointer. Only primitive completion values cross the boundary.
    pub(crate) shadow_realms: crate::fasthash::FastMap<usize, Box<Interp>>,
    /// DataView state `(buffer ptr, byteOffset, byteLength)`, keyed by the DataView's pointer.
    /// DataView state: (buffer ptr, byteOffset, byteLength, is-length-tracking).
    pub(crate) data_views: crate::fasthash::FastMap<usize, (usize, usize, usize, bool)>,
    /// Compiled regular expressions, keyed by the RegExp object's pointer.
    pub(crate) regexps: crate::fasthash::FastMap<usize, Rc<crate::regex::Regex>>,
    /// Proxy `(target, handler)` pairs, keyed by the proxy object's pointer.
    pub(crate) proxies: crate::fasthash::FastMap<usize, (Value, Value)>,
    /// The active `new.target` for the function currently executing (Undefined outside a `new`).
    pub(crate) new_target: Value,
    /// The `new.target` to install for the next constructor invocation (set by `construct`).
    pub(crate) pending_new_target: Value,
    /// The `$262.IsHTMLDDA` objects, one per realm (emulate `document.all`): typeof "undefined",
    /// falsy, and loosely equal to undefined/null, despite being callable Objects.
    pub(crate) htmldda: Vec<Gc>,
    /// The caller's realm during a cross-realm [[Construct]]: the spec pops the callee context
    /// before throwing a derived constructor's return/`this` validation errors, so those errors
    /// belong to the caller's realm.
    pub(crate) ctor_caller_realm: Option<RealmState>,
    /// Additional realms created via `$262.createRealm()`, keyed by the realm's global-object pointer.
    /// Each holds its own intrinsics; the shared side tables (proxies, buffers, …) and well-known
    /// symbols are common, so objects cross realm boundaries freely.
    pub(crate) realms: crate::fasthash::FastMap<usize, RealmState>,
    /// Promise state keyed by the promise object's pointer.
    pub(crate) promises: crate::fasthash::FastMap<usize, PromiseState>,
    /// Rejected promises with no handler attached at rejection time (ptr -> reason). Removed when a
    /// handler is later attached; whatever remains after a microtask checkpoint is a genuine
    /// unhandled rejection the embedder can report (see [`Engine::take_unhandled_rejections`]).
    /// Keyed by the promise object's pointer; holds `(promise, reason)` so the runtime can hand
    /// both to a global `unhandledrejection` handler.
    pub(crate) unhandled_rejections: crate::fasthash::FastMap<usize, (Value, Value)>,
    /// Temporal object internal slots, keyed by the object's pointer.
    pub(crate) temporal: crate::fasthash::FastMap<usize, crate::temporal::Temporal>,
    /// The calendar id of a Temporal date-bearing object (default "iso8601"), keyed by object ptr.
    pub(crate) temporal_cal: crate::fasthash::FastMap<usize, std::rc::Rc<str>>,
    /// The microtask queue (drained after the main script by [`crate::Engine::eval`]).
    pub(crate) microtasks: std::collections::VecDeque<Job>,
    /// Embedder host state (typed slots + resource table); see [`crate::host`]. Reached from
    /// native fns via [`Interp::op_state`] — the only way, since `NativeFn` cannot capture.
    pub(crate) host_state: crate::host::OpState,
    /// Live generator coroutines, keyed by the generator object's pointer. Each owns an OS thread
    /// that runs the body and parks at every `yield` (see [`crate::coroutine`]).
    pub(crate) generators: crate::fasthash::FastMap<usize, crate::coroutine::Coroutine>,
    /// Live-object count above which the next allocation safe point runs the cycle collector.
    pub(crate) gc_next: i64,
    /// Call counter for [`Interp::gc_check_amortized`]: the JIT fast call polls the (TLS-backed)
    /// live-object counters only every 16th call — a bounded delay that costs at most 16 calls'
    /// worth of allocation slack against `gc_next`'s doubling schedule.
    pub(crate) gc_tick: u32,
    /// Prune the scope registry once its entry count passes this floating threshold.
    pub(crate) scope_gc_next: usize,
    /// True while a native constructor is being invoked via `new` (lets e.g. `Number`/`String`
    /// build a wrapper object instead of returning a primitive).
    pub(crate) constructing: bool,
    /// True only while a *derived* class constructor body is executing — the one context where a
    /// `super(...)` call is legal. Field initializers, methods, plain functions and global code all
    /// clear it, so a stray `super()` (including one reached through a direct `eval`) is rejected
    /// instead of re-entering instance-field initialization unboundedly.
    pub(crate) super_call_ok: bool,
    /// Live user-function activations, oldest first: (fn ptr, fn object, arguments object,
    /// strict). Backs the legacy SpiderMonkey `fn.caller` / `fn.arguments` reflection.
    pub(crate) fn_frames: Vec<FnFrame>,
    /// Monotonic [[AsyncEvaluationOrder]] source for module evaluation.
    pub(crate) module_async_seq: u64,
    /// Set by `yield*` just before parking: the yielded value is the inner iterator's result
    /// object and must pass through the generator driver unwrapped.
    pub(crate) yield_raw_result: bool,
    /// True while statements *directly* in an async generator's body run (nested ordinary
    /// functions clear it; arrows inherit): `return <expr>` awaits its value only there.
    pub(crate) in_async_gen_body: bool,
    /// NamedEvaluation: the binding/property name an anonymous class expression being evaluated
    /// should take — applied *before* its static initializers run.
    pub(crate) pending_fn_name: Option<String>,
    /// A pending proper tail call: set by `return f(...)` in a strict ordinary function; the
    /// nearest `Interp::call` frame re-dispatches it (a trampoline), keeping the stack flat.
    pub(crate) pending_tail: Option<Box<(Value, Value, Vec<Value>)>>,
    /// True while executing a body where `return f(...)` is a proper tail call (a strict,
    /// ordinary, non-constructor function).
    pub(crate) tco_ok: bool,
    /// GetTemplateObject cache: one frozen strings array per tagged-template *site* (AST node),
    /// so the same site always passes the identical object. Keyed by the quasis slice address.
    pub(crate) template_cache: std::collections::HashMap<usize, Value>,
    /// True while class field initializer code runs: a direct eval from there may not contain an
    /// `arguments` reference (arrows inherit the context; ordinary functions clear it).
    pub(crate) in_field_init_code: bool,
    /// A stack of disposal scopes — one frame per block/function body that can hold `using`
    /// declarations. Resources are disposed in reverse on scope exit (see `dispose_frame`).
    pub(crate) using_stack: Vec<Vec<Disposable>>,
    /// Monotonic counter minting globally-unique private backing keys for auto-accessors (so a
    /// subclass's accessor never collides with a superclass's on the same instance).
    pub(crate) accessor_seq: u64,
    /// Scratch collector for `context.addInitializer(fn)` calls during decorator application; drained
    /// after each decorator runs.
    pub(crate) decorator_initializers: Vec<Value>,
    /// Annex B.3.3 web-compat function hoisting: block/if-position function declarations (in sloppy
    /// code) whose names got a function-scope `var` binding, keyed by AST pointer. When such a
    /// declaration is *evaluated*, the block binding's value is copied into the var binding. The
    /// `Rc` value keeps the AST alive so a pointer is never reused by a different function.
    pub(crate) annexb_fn_sync: crate::fasthash::FastMap<usize, Rc<Function>>,
    /// `import defer` namespaces awaiting first access: namespace object ptr → module key.
    /// Accessing a property of one evaluates the module (then the entry is removed).
    pub(crate) deferred_ns: crate::fasthash::FastMap<usize, String>,
    /// module key → its (single) deferred namespace object.
    pub(crate) deferred_ns_objs: crate::fasthash::FastMap<String, Value>,
    /// Promise-state forwarding: when a native super() grafts a fresh promise's state onto the
    /// subclass `this`, the original object's resolvers redirect here.
    pub(crate) promise_forward: crate::fasthash::FastMap<usize, Value>,
    /// Mapped `arguments` objects: object ptr → (function scope, per-index parameter name — None
    /// once unmapped by delete/defineProperty). Reads/writes of still-mapped indices alias the
    /// parameter bindings.
    pub(crate) mapped_arguments: crate::fasthash::FastMap<usize, (Env, Vec<Option<String>>)>,
    /// Source-phase imports: canonical module key → its (cached) ModuleSource object.
    pub(crate) module_source_objs: crate::fasthash::FastMap<String, Value>,
    /// FinalizationRegistry registrations (ptr → unregister tokens), for `unregister`'s result.
    pub(crate) fr_tokens: crate::fasthash::FastMap<usize, Vec<Value>>,
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
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_EVAL_DEPTH: u32 = 1500;
/// On wasm32 the ceiling is the engine's *host* call stack (V8's, not raisable from content):
/// measured in Chrome, the simplest interpreted frame overflows it near depth ~220, and heavier
/// frames die sooner — 128 keeps the guard firing as a clean RangeError before the host stack
/// does.
#[cfg(target_arch = "wasm32")]
pub const MAX_EVAL_DEPTH: u32 = 128;

/// Live-object ceiling (≈ a few hundred MB). When a safe point sees this many *live* objects, the
/// cycle collector runs; if it can't get back under, a RangeError is thrown rather than exhausting
/// RAM. This bounds genuine retention; transient cyclic garbage is reclaimed and doesn't count.
pub const MAX_LIVE: i64 = 3_000_000;

/// Live-object count at which the collector first runs; the threshold then floats (see `gc_check`).
pub const GC_TRIGGER: i64 = 200_000;
/// Scope-registry entry count that arms a registry prune.
const SCOPE_GC_TRIGGER: usize = 250_000;

/// Memory safety valves. lumen has no garbage collector and several built-ins iterate/allocate in
/// proportion to a user-controlled `length`, so without these a single adversarial test (e.g.
/// `Array(4e9).join()` or `s += s` doubling a string) can exhaust all RAM. Operations that would
/// materialize more than these bounds raise a RangeError instead. They are generous relative to
/// real test262 tests but small enough that one runaway test stays bounded.
pub const MAX_ARRAY_OP_LEN: usize = 1 << 20; // ~1M elements

/// Byte ceiling for a single ArrayBuffer/SharedArrayBuffer allocation (real programs allocate
/// tens of MB; runaway growth is still stopped well below the process heap cap).
pub const MAX_BUFFER_BYTES: usize = 1 << 28; // 256 MiB
                                             // Large enough for suite tests that build 2^24-char escape strings, small enough that runaway
                                             // string growth still dies as a RangeError rather than an OOM.
pub const MAX_STR_LEN: usize = 1 << 26; // ~67M

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
    pub error_protos: crate::fasthash::FastMap<&'static str, Gc>,
    pub eval_fn: Option<Gc>,
    pub extra_protos: crate::fasthash::FastMap<&'static str, Gc>,
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
    pub(crate) fn create_realm(&mut self) -> Value {
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
        string_proto.borrow_mut().exotic = Exotic::StrWrap("".into());
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
        self.error_protos = Default::default();
        self.extra_protos = Default::default();
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
            self.eval_realm_fns.insert(Rc::as_ptr(ef) as usize);
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
    pub(crate) fn new() -> Interp {
        let object_proto = Object::new(None);
        let function_proto = Object::new(Some(object_proto.clone()));
        let array_proto = Object::new(Some(object_proto.clone()));
        let string_proto = Object::new(Some(object_proto.clone()));
        let number_proto = Object::new(Some(object_proto.clone()));
        let boolean_proto = Object::new(Some(object_proto.clone()));
        // These prototypes are themselves wrapper exotics with default primitive data, so e.g.
        // `Number.prototype.valueOf()` / `Number.prototype == 0` work.
        string_proto.borrow_mut().exotic = Exotic::StrWrap("".into());
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
            error_protos: Default::default(),
            sym_counter: 0,
            sym_registry: Default::default(),
            console: Vec::new(),
            strict: false,
            depth: 0,
            class_info: Default::default(),
            eval_fn: None,
            eval_realm_fns: Default::default(),
            elems_protector: std::cell::Cell::new((0, false)),
            construct_ics: Default::default(),
            iterator_sym: None,
            wk_syms: Vec::new(),
            short_circuit: false,
            import_meta: None,
            import_base: String::new(),
            modules: Default::default(),
            module_recs: Default::default(),
            module_loader: None,
            module_ns: Default::default(),
            tier: match std::env::var("LUMEN_TIER").as_deref() {
                Ok("bytecode") => crate::bytecode::Tier::Bytecode,
                Ok("jit") => crate::bytecode::Tier::Jit,
                _ => crate::bytecode::Tier::Interp,
            },
            tier_threshold: std::env::var("LUMEN_TIER_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            vm_pool: Vec::new(),
            stub_cache: vec![std::cell::Cell::new(StubEntry::default()); STUB_CACHE_SIZE],
            frame_pool: FramePool(Vec::new()),
            creation_pins: Default::default(),
            global_env_pins: Vec::new(),
            cur_coro: 0,
            jit_helpers: crate::jit::helper_table(),
            jit_layout: std::cell::OnceCell::new(),
            interp_layout: std::cell::Cell::new(InterpLayout::default()),
            inline_ic_safe: std::cell::Cell::new(true),
            str_units: Vec::new(),
            re_texts: Vec::new(),
            regexp_last: None,
            map_data: Default::default(),
            extra_protos: Default::default(),
            array_buffers: Default::default(),
            shared_buffers: Default::default(),
            immutable_buffers: std::collections::HashSet::new(),
            can_block: true,
            pending_async_waits: Vec::new(),
            pending_timers: Vec::new(),
            agent: None,
            typed_arrays: Default::default(),
            async_gens: std::collections::HashSet::new(),
            global_var_names: std::collections::HashSet::new(),
            gc_pins: Default::default(),
            ta_buffer: Default::default(),
            shadow_realms: Default::default(),
            data_views: Default::default(),
            regexps: Default::default(),
            proxies: Default::default(),
            new_target: Value::Undefined,
            pending_new_target: Value::Undefined,
            htmldda: Vec::new(),
            ctor_caller_realm: None,
            realms: Default::default(),
            promises: Default::default(),
            unhandled_rejections: Default::default(),
            temporal: Default::default(),
            temporal_cal: Default::default(),
            microtasks: std::collections::VecDeque::new(),
            host_state: Default::default(),
            generators: Default::default(),
            gc_next: GC_TRIGGER,
            gc_tick: 0,
            scope_gc_next: SCOPE_GC_TRIGGER,
            constructing: false,
            super_call_ok: false,
            fn_frames: Vec::new(),
            module_async_seq: 0,
            in_field_init_code: false,
            yield_raw_result: false,
            in_async_gen_body: false,
            pending_fn_name: None,
            pending_tail: None,
            tco_ok: false,
            template_cache: Default::default(),
            using_stack: Vec::new(),
            accessor_seq: 0,
            decorator_initializers: Vec::new(),
            annexb_fn_sync: Default::default(),
            deferred_ns: Default::default(),
            deferred_ns_objs: Default::default(),
            promise_forward: Default::default(),
            mapped_arguments: Default::default(),
            module_source_objs: Default::default(),
            fr_tokens: Default::default(),
            async_gen_busy: std::collections::HashSet::new(),
            async_gen_queue: Default::default(),
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
        obj.borrow_mut().exotic = Exotic::Error(self.capture_stack());
        let msg = message.into();
        if !msg.is_empty() {
            obj.borrow_mut()
                .props
                .insert("message", Property::builtin(Value::from_string(msg)));
        }
        Value::Obj(obj)
    }
    /// Snapshot the current call stack as the `\n    at <fn>` lines for an error's `stack`.
    /// Innermost frame first (Node order). We are a tree-walker without per-call source spans, so
    /// frames carry the function name only (`<anonymous>` when unnamed); the `stack` getter adds
    /// the `name: message` head. Bounded by the engine's own recursion guard (~128 frames).
    fn capture_stack(&self) -> Rc<str> {
        let mut out = String::new();
        for frame in self.fn_frames.iter().rev() {
            let callee = frame.callee();
            let name = {
                let b = callee.borrow();
                match b.props.get("name").map(|p| &p.value) {
                    Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
                    _ => None,
                }
            }
            .unwrap_or_else(|| "<anonymous>".to_string());
            out.push_str("\n    at ");
            out.push_str(&name);
        }
        Rc::from(out.as_str())
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
    pub(crate) fn ta_read(&self, info: &TaInfo, idx: usize) -> Value {
        if idx >= self.ta_len(info).unwrap_or(0) {
            return Value::Undefined;
        }
        let es = info.kind.elsize();
        let start = info.offset + idx * es;
        let decode = |buf: &[u8]| -> Value {
            if start + es <= buf.len() {
                let bytes = &buf[start..start + es];
                if info.kind.is_bigint() {
                    Value::BigInt(info.kind.read_bigint(bytes).into())
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

    /// The raw bytes of `nelems` elements starting at element `elem_start` (None when the view
    /// is detached/out-of-bounds). Used for bitwise same-type copies that must preserve NaN bits.
    pub(crate) fn ta_read_bytes(
        &self,
        info: &TaInfo,
        elem_start: usize,
        nelems: usize,
    ) -> Option<Vec<u8>> {
        let es = info.kind.elsize();
        let start = info.offset + elem_start * es;
        let end = start + nelems * es;
        let take = |buf: &[u8]| {
            if end <= buf.len() {
                Some(buf[start..end].to_vec())
            } else {
                None
            }
        };
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            return shared_mem_get(id).and_then(|mem| take(&mem.lock().unwrap()));
        }
        self.array_buffers.get(&info.buffer).and_then(|b| take(b))
    }

    /// Write raw bytes at element `elem_start` (bounds-checked; silently drops what doesn't fit).
    pub(crate) fn ta_write_bytes(&mut self, info: &TaInfo, elem_start: usize, bytes: &[u8]) {
        let start = info.offset + elem_start * info.kind.elsize();
        let put = |buf: &mut [u8]| {
            if start + bytes.len() <= buf.len() {
                buf[start..start + bytes.len()].copy_from_slice(bytes);
            }
        };
        if let Some(&id) = self.shared_buffers.get(&info.buffer) {
            if let Some(mem) = shared_mem_get(id) {
                put(&mut mem.lock().unwrap());
            }
            return;
        }
        if let Some(b) = self.array_buffers.get_mut(&info.buffer) {
            put(b);
        }
    }

    /// Atomically read-modify-write an integer TypedArray element: `f` maps the old raw value to
    /// the new one (or `None` for no write, as in a failed compareExchange). For a shared buffer
    /// the whole operation happens under one lock hold, so concurrent agents can't interleave
    /// (a lost `Atomics.add` increment would spin another agent's waitUntil loop forever).
    /// Returns the old value, or `None` when the index is out of range.
    pub(crate) fn ta_modify(
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
    pub(crate) fn ta_store(&mut self, info: &TaInfo, idx: usize, v: &Value) -> Result<(), Abrupt> {
        // IntegerIndexedElementSet: the value coerces FIRST (observable, spec order); a write
        // into an immutable buffer then fails like a non-writable data property — a TypeError in
        // strict mode, a silent no-op in sloppy.
        let immutable =
            !self.immutable_buffers.is_empty() && self.immutable_buffers.contains(&info.buffer);
        if info.kind.is_bigint() {
            let n = self.to_bigint(v)?;
            if !immutable {
                self.ta_write_bigint(info, idx, n.to_i128_wrapping());
            }
        } else {
            let n = self.to_number(v)?;
            if !immutable {
                self.ta_write(info, idx, n);
            }
        }
        if immutable && self.strict {
            return Err(self.throw(
                "TypeError",
                "Cannot write to a TypedArray over an immutable ArrayBuffer",
            ));
        }
        Ok(())
    }

    /// Write a BigInt (i128) into element `idx` (out-of-range writes are ignored).
    pub(crate) fn ta_write_bigint(&mut self, info: &TaInfo, idx: usize, n: i128) {
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
    pub(crate) fn ta_write(&mut self, info: &TaInfo, idx: usize, n: f64) {
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
    pub(crate) fn sym_key(data: &SymbolData) -> String {
        format!("\u{0}{}", data.id)
    }
    pub(crate) fn is_sym_key(key: &str) -> bool {
        // `\0<digits>` (symbol slot) or `\0<lowercase word>` (internal marker) — a user-authored
        // string key that merely begins with NUL (legal in JS) must not be mistaken for either.
        key.strip_prefix('\u{0}').is_some_and(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'_' || b == b'-')
        })
    }
    /// Whether `key` is an internal private-element key. Every runtime private name carries a
    /// `\u{1}<serial>` suffix (auto-accessor backings a `\u{0}` marker), so a user property whose
    /// *string* name merely starts with `#` (a computed key) is not mistaken for one.
    pub(crate) fn is_private_key(key: &str) -> bool {
        key.starts_with('#') && (key.contains('\u{1}') || key.contains('\u{0}'))
    }
    /// Recover the symbol `Value` behind an internal symbol key (for `getOwnPropertySymbols`).
    pub(crate) fn sym_from_key(&self, key: &str) -> Option<Value> {
        let id: u64 = key.strip_prefix('\u{0}')?.parse().ok()?;
        self.sym_registry.get(&id).map(|d| Value::Sym(d.clone()))
    }

    // ----- object construction ----------------------------------------------------------------

    pub fn new_object(&self) -> Gc {
        Object::new(Some(self.object_proto.clone()))
    }

    /// The global object (`globalThis`) as a value — for embedders that must expose it
    /// (N-API's `napi_get_global`).
    pub fn global_this(&self) -> Value {
        Value::Obj(self.global.clone())
    }

    /// [[Get]] for embedders: like [`Self::get_member`] but surfaces a thrown value rather than
    /// the crate-internal `Abrupt` completion, so op crates can propagate it as `Err(Value)`.
    pub fn member_get(&mut self, base: &Value, key: &str) -> Result<Value, Value> {
        self.get_member(base, key).map_err(|a| match a {
            Abrupt::Throw(v) => v,
            _ => self.make_error(
                "Error",
                "unexpected non-throw completion during property get",
            ),
        })
    }

    /// [[Set]] for embedders (see [`Self::member_get`]).
    pub fn member_set(&mut self, base: &Value, key: &str, value: Value) -> Result<(), Value> {
        self.set_member(base, key, value).map_err(|a| match a {
            Abrupt::Throw(v) => v,
            _ => self.make_error(
                "Error",
                "unexpected non-throw completion during property set",
            ),
        })
    }

    // ----- embed helpers for host addons (N-API) ---------------------------------------------

    /// A stable identity for an object value (its heap address), or `None` for a non-object.
    /// Used by host addons to key per-object native state (`napi_wrap`) and to implement `===`
    /// on objects without exposing the object handle.
    pub fn object_addr(&self, v: &Value) -> Option<usize> {
        v.as_obj().map(|o| Rc::as_ptr(o) as usize)
    }

    /// `Object.getPrototypeOf(v)` — the value's `[[Prototype]]` (`null` when there is none).
    pub fn prototype_of(&self, v: &Value) -> Value {
        match v.as_obj().and_then(|o| o.borrow().proto.clone()) {
            Some(p) => Value::Obj(p),
            None => Value::Null,
        }
    }

    /// Whether `v` is an Error object (has the error exotic on itself or anywhere up its
    /// prototype chain — so subclass instances count too).
    pub fn is_error_value(&self, v: &Value) -> bool {
        let mut cur = v.as_obj().cloned();
        while let Some(o) = cur {
            if matches!(o.borrow().exotic, Exotic::Error(_)) {
                return true;
            }
            cur = o.borrow().proto.clone();
        }
        false
    }

    /// The `===` (SameValueNonNumeric / strict-equality) predicate, for host addons.
    pub fn values_strict_equal(&self, a: &Value, b: &Value) -> bool {
        self.strict_equals(a, b)
    }

    /// Whether the current native call is a construct (`new`) — a native constructor uses this to
    /// build its instance only under `new`.
    pub fn is_constructing(&self) -> bool {
        self.constructing
    }

    /// A fresh object with an explicit `[[Prototype]]` (`null` → no prototype). For host addons
    /// creating instances with a class's prototype.
    pub fn new_object_with_proto(&self, proto: &Value) -> Value {
        Value::Obj(Object::new(proto.as_obj().cloned()))
    }

    /// Mark a native function as a constructor and wire up its `prototype`/`constructor` link, so
    /// `new ctor()` is legal and instances inherit from `proto`. Both links are non-enumerable,
    /// as for a JS `class`.
    pub fn set_constructor_prototype(&self, ctor: &Value, proto: &Value) {
        if let Some(c) = ctor.as_obj() {
            c.borrow_mut().is_constructor = true;
            c.borrow_mut().props.insert(
                "prototype",
                Property::data(proto.clone(), false, false, false),
            );
        }
        if let Some(p) = proto.as_obj() {
            p.borrow_mut().props.insert(
                "constructor",
                Property::data(ctor.clone(), true, false, true),
            );
        }
    }

    /// Define an accessor (getter/setter) property on `target`. For host addons registering class
    /// accessors (`napi_define_class` with getter/setter descriptors).
    pub fn define_accessor_value(
        &self,
        target: &Value,
        name: &str,
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
    ) {
        if let Some(o) = target.as_obj() {
            o.borrow_mut()
                .props
                .insert(name, Property::accessor_prop(get, set, enumerable, true));
        }
    }

    /// A TypedArray's `(napi element-type code, byte length, data pointer)`, or `None` for a
    /// non-typed-array. The pointer aliases the live backing store; it is valid until the buffer
    /// is resized or detached. For `napi_get_typedarray_info`.
    pub fn typed_array_raw(&mut self, v: &Value) -> Option<(u8, usize, *mut u8)> {
        let obj = v.as_obj()?;
        let info = self
            .typed_arrays
            .get(&(Rc::as_ptr(obj) as usize))
            .copied()?;
        let len = self.ta_len(&info)?;
        let (code, elem) = match info.kind {
            crate::value::TaKind::I8 => (0u8, 1usize),
            crate::value::TaKind::U8 => (1, 1),
            crate::value::TaKind::U8Clamped => (2, 1),
            crate::value::TaKind::I16 => (3, 2),
            crate::value::TaKind::U16 => (4, 2),
            crate::value::TaKind::I32 => (5, 4),
            crate::value::TaKind::U32 => (6, 4),
            crate::value::TaKind::F32 => (7, 4),
            crate::value::TaKind::F64 => (8, 8),
            crate::value::TaKind::I64 => (9, 8),
            crate::value::TaKind::U64 => (10, 8),
            crate::value::TaKind::F16 => (1, 2), // no N-API code for float16; report bytes
        };
        let byte_len = len * elem;
        let buf = self.array_buffers.get_mut(&info.buffer)?;
        let ptr = unsafe { buf.as_mut_ptr().add(info.offset) };
        Some((code, byte_len, ptr))
    }

    pub fn make_array(&self, items: Vec<Value>) -> Value {
        let obj = Object::new(Some(self.array_proto.clone()));
        {
            let b = obj.borrow_mut();
            b.props.mark_array();
        }
        obj.borrow_mut().exotic = Exotic::Array;
        let len = items.len();
        {
            let mut b = obj.borrow_mut();
            for v in items {
                b.props.push_dense(Property::plain(v));
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

    /// Invoke a native callable (bare `fn` or data-carrying closure) — the shared body for the
    /// call/construct/super dispatch paths.
    pub(crate) fn dispatch_native(
        &mut self,
        call: &Callable,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        match call {
            Callable::Native(f) => f(self, this, args),
            Callable::NativeData(f) => {
                let f = f.clone();
                f(self, this, args)
            }
            _ => unreachable!("dispatch_native on a non-native callable"),
        }
    }

    /// A function `Value` backed by a data-carrying native closure — the embedder API for host
    /// functions that must capture state (N-API callbacks carrying a C fn pointer + `void*`).
    pub fn new_native_fn(
        &self,
        name: &str,
        len: usize,
        f: std::rc::Rc<crate::value::NativeClosure>,
    ) -> Value {
        Value::Obj(self.make_native_closure(name, len, f))
    }

    /// A function object backed by a data-carrying native closure (see [`NativeClosure`]). Like
    /// [`make_native`], but the callable can capture host state (used for N-API functions).
    pub fn make_native_closure(
        &self,
        name: &str,
        len: usize,
        f: std::rc::Rc<crate::value::NativeClosure>,
    ) -> Gc {
        let obj = Object::new(Some(self.function_proto.clone()));
        {
            let mut b = obj.borrow_mut();
            b.call = Callable::NativeData(f);
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

    /// Embedder host state (typed per-subsystem slots + the resource table). This is how a
    /// native fn reaches Rust state: `NativeFn` is a bare `fn` pointer and cannot capture.
    pub fn op_state(&mut self) -> &mut crate::host::OpState {
        &mut self.host_state
    }

    /// Shorthand for `op_state().get_mut::<T>()`.
    pub fn host_mut<T: std::any::Any>(&mut self) -> Option<&mut T> {
        self.host_state.get_mut()
    }

    /// Shorthand for `op_state().resources`.
    pub fn resource_table(&mut self) -> &mut crate::host::ResourceTable {
        &mut self.host_state.resources
    }

    /// [`call`](Interp::call) with an abrupt completion lowered to the thrown value — the
    /// shape a [`NativeFn`] returns, for native fns that call back into JS.
    pub fn invoke(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        self.call(callee, this, args).map_err(abrupt_value)
    }

    /// [`construct`](Interp::construct) with an abrupt completion lowered to the thrown value —
    /// the embed-friendly `new callee(...args)` (used by N-API's `napi_new_instance` /
    /// `napi_create_promise`).
    pub fn construct_value(&mut self, callee: Value, args: &[Value]) -> Result<Value, Value> {
        self.construct(callee, args).map_err(abrupt_value)
    }

    /// ToNumber with the abrupt completion lowered to the thrown value (see [`invoke`]).
    /// Conversions can throw: they may run user `valueOf`/`toString`.
    pub fn coerce_number(&mut self, v: &Value) -> Result<f64, Value> {
        self.to_number(v).map_err(abrupt_value)
    }

    /// ToString with the abrupt completion lowered to the thrown value (see [`invoke`]).
    pub fn coerce_string(&mut self, v: &Value) -> Result<Rc<str>, Value> {
        self.to_string(v).map(|s| (&s).into()).map_err(abrupt_value)
    }

    /// The bytes a TypedArray view covers (`None` when `v` isn't a typed array or its buffer
    /// is detached). The embedder's binary bridge: encoders/crypto/fetch move bytes through
    /// this.
    pub fn typed_array_bytes(&self, v: &Value) -> Option<Vec<u8>> {
        let obj = v.as_obj()?;
        let info = self
            .typed_arrays
            .get(&(Rc::as_ptr(obj) as usize))
            .copied()?;
        let len = self.ta_len(&info)?;
        self.ta_read_bytes(&info, 0, len)
    }

    /// Overwrite a TypedArray's covered bytes from the start (a write past the view's end is
    /// a bounds-checked no-op, matching the engine's internal write semantics). `false` when
    /// `v` isn't a typed array.
    pub fn typed_array_set_bytes(&mut self, v: &Value, bytes: &[u8]) -> bool {
        let Some(obj) = v.as_obj() else {
            return false;
        };
        let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(obj) as usize)).copied() else {
            return false;
        };
        self.ta_write_bytes(&info, 0, bytes);
        true
    }

    /// A fresh `Uint8Array` holding `bytes`, constructed through the realm's own
    /// `Uint8Array` constructor.
    pub fn make_uint8array(&mut self, bytes: &[u8]) -> Result<Value, Value> {
        let global = Value::Obj(self.global.clone());
        let ctor = self
            .get_member(&global, "Uint8Array")
            .map_err(abrupt_value)?;
        let ta = self
            .construct(ctor, &[Value::Num(bytes.len() as f64)])
            .map_err(abrupt_value)?;
        self.typed_array_set_bytes(&ta, bytes);
        Ok(ta)
    }

    pub(crate) fn make_function(&self, func: Rc<Function>, env: Env) -> Value {
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
                crate::value::fn_key(0), // "length"
                Property::data(Value::Num(arity as f64), false, false, true),
            );
            b.props.insert(
                crate::value::fn_key(1), // "name"
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
                proto.borrow_mut().props.insert(
                    crate::value::fn_key(3), // "constructor"
                    Property::builtin(Value::Obj(obj.clone())),
                );
            }
            obj.borrow_mut().props.insert(
                crate::value::fn_key(2), // "prototype"
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

    /// Whether `o` may take the dense element fast paths: a plain object or array — not an
    /// exotic (wrapper/arguments), module namespace, or deferred namespace, whose reads/writes
    /// have extra semantics the fast paths would skip. (Typed arrays and proxies keep their
    /// indexed data out of `props`, so a dense hit on them is impossible and they need no guard.)
    #[inline]
    /// Whether appending a NEW element to array `o` can skip the OrdinarySet prototype walk:
    /// `o`'s prototype is exactly the realm's `Array.prototype` and the protector proof holds
    /// (see [`Interp::elems_protector`]). The caller has already verified `o` itself.
    pub(crate) fn array_append_unshadowed(&self, o: &Gc) -> bool {
        {
            let b = o.borrow();
            match b.proto.as_ref() {
                Some(p) if Rc::ptr_eq(p, &self.array_proto) => {}
                _ => return false,
            }
        }
        let epoch = crate::value::proto_epoch();
        if epoch == u32::MAX {
            return false; // permanently invalidated — never trust the cache
        }
        let (e, ok) = self.elems_protector.get();
        if e == epoch {
            return ok;
        }
        let no_index_keys = |g: &Gc| {
            g.borrow().props.iter().all(|(k, _)| {
                !k.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
                    || crate::value::canonical_index(k).is_none()
            })
        };
        let ap = &self.array_proto;
        let op = &self.object_proto;
        let ok = self.ordinary_get_ptr(Rc::as_ptr(ap) as usize)
            && self.ordinary_get_ptr(Rc::as_ptr(op) as usize)
            && matches!(ap.borrow().proto.as_ref(), Some(p) if Rc::ptr_eq(p, op))
            && op.borrow().proto.is_none()
            && matches!(op.borrow().exotic, Exotic::None)
            && no_index_keys(ap)
            && no_index_keys(op)
            && {
                // Future structural inserts on either proto must bump the epoch this proof is
                // keyed on (plain `Array.prototype[0] = v` isn't a defineProperty).
                ap.borrow().props.mark_proto();
                op.borrow().props.mark_proto();
                true
            };
        self.elems_protector.set((epoch, ok));
        ok
    }

    fn plain_for_elems(&self, o: &Gc) -> bool {
        if !matches!(o.borrow().exotic, Exotic::Array | Exotic::None) {
            return false;
        }
        if !self.module_ns.is_empty() || !self.deferred_ns.is_empty() {
            let ptr = Rc::as_ptr(o) as usize;
            if self.module_ns.contains_key(&ptr) || self.deferred_ns.contains_key(&ptr) {
                return false;
            }
        }
        true
    }

    /// `o[n]` read fast path: an own dense data element on a plain object/array, fetched without
    /// stringifying or hashing the index. `None` means "take the generic path" (miss, accessor,
    /// exotic receiver, non-index number) — never "absent".
    #[inline]
    pub(crate) fn fast_get_elem(&mut self, o: &Gc, n: f64) -> Option<Value> {
        if n.trunc() != n || !(0.0..u32::MAX as f64).contains(&n) {
            return None;
        }
        if !self.plain_for_elems(o) {
            return None;
        }
        let b = o.borrow();
        // One-load mirror hit (see `Props::mirror`); a miss only means "answer classically".
        if let Some(f) = b.props.mirror_get(n as u32) {
            return Some(Value::Num(f));
        }
        let p = b.props.get_index(n as u32)?;
        if p.accessor {
            return None;
        }
        Some(p.value.clone())
    }

    /// `o[n] = v` write fast path: overwrite an existing own writable dense data element.
    /// Correct regardless of the prototype chain — an own writable data property always wins
    /// OrdinarySet. Returns the value back on miss so the caller runs the generic path.
    #[inline]
    pub(crate) fn fast_set_elem(&mut self, o: &Gc, n: f64, v: Value) -> Result<(), Value> {
        if n.trunc() != n || !(0.0..u32::MAX as f64).contains(&n) {
            return Err(v);
        }
        if !self.plain_for_elems(o) {
            return Err(v);
        }
        let mut b = o.borrow_mut();
        // Mirror-coherent overwrite (does NOT invalidate the mirror like a raw get_index_mut).
        b.props.set_index_value(n as u32, v)
    }

    /// `obj.name` read with a per-site inline cache (bytecode `GetProp`/`GetMethod`). The cache
    /// holds where `name` was last found at this site: an own `entries` slot, or a slot on the
    /// object `depth` prototype hops up (methods!). A hit re-validates everything it relies on —
    /// each hop below the holder is hash-checked to still lack an own `name`, the holder slot is
    /// key-compared — so staleness costs a re-derivation, never a wrong answer. Anything not a
    /// plain data property on an ordinary (or array) chain falls through to full `[[Get]]`.
    pub(crate) fn get_prop_ic(
        &mut self,
        base: &Value,
        name: &str,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> Result<Value, Abrupt> {
        match base {
            Value::Obj(o) => {
                if let Some(v) = self.try_ic_get(o, name, cache) {
                    return Ok(v);
                }
            }
            // Primitive receivers: named non-index reads (`"x".replace`, `(1).toFixed`) resolve
            // on the wrapper prototype chain — cache that walk too. `length` and numeric keys
            // have primitive-specific handling in get_member; symbols get description handling.
            Value::Str(_) | Value::Num(_) | Value::Bool(_)
                if name != "length"
                    && name != "description"
                    && !name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) =>
            {
                let proto = match base {
                    Value::Str(_) => self.string_proto.clone(),
                    Value::Num(_) => self.number_proto.clone(),
                    _ => self.boolean_proto.clone(),
                };
                if let Some(v) = self.try_ic_get(&proto, name, cache) {
                    return Ok(v);
                }
            }
            _ => {}
        }
        self.get_member(base, name)
    }

    /// Whether object pointer `ptr` (holding `b`) reads like an ordinary object for IC purposes.
    /// `Exotic::Array` is allowed: array *named* reads are ordinary (`length` is a real stored
    /// property); everything else exotic (string wrappers, arguments, …) and anything in a side
    /// table (proxy, namespace, typed array) is not.
    #[inline]
    fn ic_plain_ptr(&self, ptr: usize, b: &Object) -> bool {
        matches!(b.exotic, Exotic::None | Exotic::Array) && self.ordinary_get_ptr(ptr)
    }

    /// The `GetProp`/`GetMethod` inline-cache fast path; `None` means "take the slow path".
    ///
    /// The prototype chain is walked by raw pointer with no `Gc` clones: every object on it is
    /// transitively kept alive by `o` (which the caller holds) for the duration of this call, and
    /// nothing mutates the chain here, so the pointers stay valid — this removes a refcount
    /// bump+drop per hop on every property access.
    fn try_ic_get(
        &self,
        o: &Gc,
        name: &str,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> Option<Value> {
        let head = Rc::as_ptr(o);
        // Two ways per site (polymorphic receivers — subclass hierarchies rotating through one
        // access site — miss a single way constantly): probe both, then the global stub cache
        // (unbounded polymorphism), then re-derive into way 1 with the old way 1 demoted.
        if let Some(v) = self.ic_shape_probe(head, name, cache.get()) {
            return Some(v);
        }
        if let Some(v) = self.ic_shape_probe(head, name, self.ic_way2(cache).get()) {
            return Some(v);
        }
        let shape = unsafe { (*head).borrow().props.shape() };
        let e = self.stub_cache[stub_slot(shape, name)].get();
        if e.name == name.as_ptr() as usize && e.st.recv_shape == shape {
            if let Some(v) = self.ic_shape_probe(head, name, e.st) {
                // Promote into the site (demoting way 1): the next access hits way-first.
                self.ic_way2(cache).set(cache.get());
                cache.set(e.st);
                return Some(v);
            }
        }
        self.ic_get_rederive(o, name, cache)
    }

    /// The second cache way of a property site (sites allocate two consecutive cells — see
    /// `Compiler::new_cache`, which keeps this pointer step in bounds).
    #[inline]
    pub(crate) fn ic_way2<'a>(
        &self,
        cache: &'a std::cell::Cell<crate::bytecode::IcState>,
    ) -> &'a std::cell::Cell<crate::bytecode::IcState> {
        unsafe { &*(cache as *const std::cell::Cell<crate::bytecode::IcState>).add(1) }
    }

    /// One way's shape fast path: a `depth == 0` (own), `depth == 1` (immediate-prototype, e.g.
    /// a method), or `depth == 2` (a subclass hierarchy hop — needs the recorded `mid_shape`)
    /// hit on non-exotic ordinary objects validates by shape-id compares alone — no per-hop
    /// key or hash checks. Shapes are shared across structurally-identical objects, so an id
    /// recorded from one instance validates the same slot on any sibling instance; a shape
    /// match on a non-holder hop proves it still *lacks* the name. Protos are re-followed
    /// live and every hop's shape re-checked, so a proto swap or hop mutation is caught.
    /// `None` = miss (no side effects): arrays-as-holders, deeper chains, accessors.
    #[inline(always)]
    fn ic_shape_probe(
        &self,
        head: *const std::cell::RefCell<Object>,
        name: &str,
        st: crate::bytecode::IcState,
    ) -> Option<Value> {
        use crate::bytecode::{IC_ARR_KEYCHK, IC_EMPTY};
        let _ = IC_EMPTY;
        // Key-checked entries (array holder — see `IC_ARR_KEYCHK`): decode the flag off the
        // depth. The `< 0x80` range check filters `IC_CREATE`/`IC_EMPTY`.
        if st.depth >= 0x80 {
            return None;
        }
        let keychk = st.depth & IC_ARR_KEYCHK != 0;
        let depth = st.depth & !IC_ARR_KEYCHK;
        if !(depth <= 1
            || (depth == 2 && st.mid_ok & 1 != 0)
            || (depth == 3 && st.mid_ok & 3 == 3))
        {
            return None;
        }
        unsafe {
            let rb = (*head).borrow();
            // An Array receiver may shape-validate only as a NON-holder (depth ≥ 1): its
            // shape proves which NAMED keys it has — so a non-digit name is provably absent
            // (element keys are all canonical indices) — but NOT where a named entry sits
            // (element entries occupy slots without transitioning the shape, so two
            // same-shape arrays can hold `length` at different slots).
            let recv_shape_ok = matches!(rb.exotic, Exotic::None)
                || (matches!(rb.exotic, Exotic::Array)
                    && (depth >= 1 || keychk)
                    && !name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()));
            if recv_shape_ok
                && self.ordinary_get_ptr(head as usize)
                && rb.props.shape() == st.recv_shape
            {
                if depth == 0 {
                    if let Some((k, p)) = rb.props.entry_at(st.slot as usize) {
                        if (!keychk || &**k == name) && !p.accessor {
                            return Some(p.value.clone());
                        }
                    }
                } else if let Some(pr) = rb.proto.as_ref() {
                    let mut hp = Rc::as_ptr(pr);
                    drop(rb); // the next hop is a different object; release the borrow
                    // Validate each intermediate hop's shape (a match proves it still lacks
                    // the name), following live protos.
                    for mid_shape in [st.mid_shape, st.mid2_shape]
                        .into_iter()
                        .take(depth.saturating_sub(1) as usize)
                    {
                        let mb = (*hp).borrow();
                        if !(matches!(mb.exotic, Exotic::None)
                            && self.ordinary_get_ptr(hp as usize)
                            && mb.props.shape() == mid_shape)
                        {
                            return None;
                        }
                        match mb.proto.as_ref() {
                            Some(p) => hp = Rc::as_ptr(p),
                            None => return None,
                        }
                    }
                    let hb = (*hp).borrow();
                    let holder_exotic_ok = matches!(hb.exotic, Exotic::None)
                        || (keychk && matches!(hb.exotic, Exotic::Array));
                    if holder_exotic_ok
                        && self.ordinary_get_ptr(hp as usize)
                        && hb.props.shape() == st.holder_shape
                    {
                        if let Some((k, p)) = hb.props.entry_at(st.slot as usize) {
                            if (!keychk || &**k == name) && !p.accessor {
                                return Some(p.value.clone());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// [`try_ic_get`]'s miss path: re-derive the resolution by a raw-pointer chain walk and
    /// refill the cache. Split out so the shape fast path's early bail-outs can tail into it.
    fn ic_get_rederive(
        &self,
        o: &Gc,
        name: &str,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> Option<Value> {
        use crate::bytecode::{IcState, IC_MAX_DEPTH};
        type ObjCell = std::cell::RefCell<Object>;
        let head = Rc::as_ptr(o);
        // Re-derive: walk the chain by raw pointer (every object is kept alive transitively by
        // `o`, nothing mutates during the read). On a plain data hit within reach record
        // (depth, slot, receiver shape, holder shape); a non-plain level or accessor defers to
        // full `[[Get]]`.
        let recv_shape = unsafe { (*head).borrow().props.shape() };
        let mut cur: *const ObjCell = head;
        // The depth-1 hop's shape, recorded when that hop is a plain (non-array) ordinary object:
        // a depth-2 fill stores it as `mid_shape` so later hits can shape-validate the whole
        // two-hop chain. An Array mid hop stays ineligible (its shape doesn't track elements, so
        // a shape match couldn't prove an index-like name is still absent).
        let mut mid = None;
        let mut mid2 = None;
        unsafe {
            for depth in 0..=IC_MAX_DEPTH {
                let b = (*cur).borrow();
                if !self.ic_plain_ptr(cur as usize, &b) {
                    return None;
                }
                if let Some(slot) = b.props.slot_of(name) {
                    let (_, p) = b.props.entry_at(slot).unwrap();
                    if p.accessor {
                        return None; // getter — must run through [[Get]]
                    }
                    let v = p.value.clone();
                    // An Array HOLDER's slot can't be trusted by shape alone: element entries
                    // occupy slots without transitioning the shape, so two same-shape arrays
                    // map the name at DIFFERENT slots. Cache it as a key-checked entry
                    // (`IC_ARR_KEYCHK`): hits re-verify the entry's key, and the JIT templates
                    // (exact depth compares, no bounds check) route it to the helper. Digit
                    // names stay uncached (element reads have their own paths).
                    let keychk = !matches!(b.exotic, Exotic::None);
                    if keychk && name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
                        return Some(v);
                    }
                    let mid_shape = if depth >= 2 { mid } else { None };
                    let mid2_shape = if depth == 3 { mid2 } else { None };
                    // Demote the previous way before refilling: a 2-shape site stabilizes
                    // with one shape per way instead of thrashing a single cell.
                    self.ic_way2(cache).set(cache.get());
                    let st = IcState {
                        depth: depth | if keychk { crate::bytecode::IC_ARR_KEYCHK } else { 0 },
                        slot: slot as u32,
                        recv_shape,
                        holder_shape: b.props.shape(),
                        mid_ok: mid_shape.is_some() as u8 | ((mid2_shape.is_some() as u8) << 1),
                        mid_shape: mid_shape.unwrap_or(0),
                        mid2_shape: mid2_shape.unwrap_or(0),
                    };
                    cache.set(st);
                    // Mirror into the stub cache so OTHER shapes rotating through this
                    // site don't evict this resolution for good.
                    self.stub_cache[stub_slot(recv_shape, name)].set(StubEntry {
                        name: name.as_ptr() as usize,
                        st,
                    });
                    return Some(v);
                }
                if depth == 1 && matches!(b.exotic, Exotic::None) {
                    mid = Some(b.props.shape());
                }
                if depth == 2 && matches!(b.exotic, Exotic::None) {
                    mid2 = Some(b.props.shape());
                }
                match b.proto.as_ref() {
                    Some(p) => cur = Rc::as_ptr(p),
                    None => return None, // chain ended: absent property — slow path
                }
            }
        }
        None
    }

    /// `obj.name = v` write with a per-site inline cache (bytecode `SetProp`/`SetPropDrop`). The
    /// fast path overwrites an existing own writable data property (OrdinarySet's winning case,
    /// correct regardless of the prototype chain); everything else defers to full `[[Set]]`.
    pub(crate) fn set_prop_ic(
        &mut self,
        base: &Value,
        name: &Rc<str>,
        v: Value,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> Result<(), Abrupt> {
        if let Value::Obj(o) = base {
            if self.try_ic_set(o, name, &v, cache) {
                return Ok(());
            }
        }
        self.set_member(base, name, v)
    }

    /// The `SetProp` inline-cache fast path; `false` means "take the slow path". Writes cache
    /// own slots only (`depth` 0): an own writable data property wins OrdinarySet regardless of
    /// the prototype chain.
    /// The fused `obj.k += v` fast path (`bytecode::Op::AppendProp`): when the property still
    /// holds the exact string the compound read produced (`lval` — nothing replaced it while the
    /// RHS evaluated), both sides are strings, and the slot is an own writable data property on a
    /// plain object, append **in place**: taking the value out of the slot and dropping the
    /// stack copy makes it uniquely referenced, so `LStr::append_in_place` (or a capacity-doubled
    /// rebuild) applies — the amortized-O(append) accumulator loop, instead of copying the whole
    /// accumulation per step. `Err(lval)` = not applicable, nothing touched, caller runs the
    /// generic Add + set.
    pub(crate) fn append_prop_fast(
        &mut self,
        o: &Gc,
        name: &str,
        lval: Value,
        x: &crate::lstr::LStr,
    ) -> Result<(), Value> {
        let Value::Str(lv) = &lval else {
            return Err(lval);
        };
        if lv.len() + x.len() > MAX_STR_LEN {
            return Err(lval); // the generic path raises the RangeError
        }
        if !self.ordinary_get_ptr(Rc::as_ptr(o) as usize) {
            return Err(lval);
        }
        let mut b = o.borrow_mut();
        if !matches!(b.exotic, Exotic::None) {
            return Err(lval);
        }
        let Some(p) = b.props.get_mut(name) else {
            return Err(lval);
        };
        if p.accessor || !p.writable {
            return Err(lval);
        }
        let same = matches!(&p.value, Value::Str(cur) if crate::lstr::LStr::ptr_eq(cur, lv));
        if !same {
            return Err(lval);
        }
        // Take the slot's handle and release the stack copy: unique unless shared elsewhere
        // (then the grow-copy path runs once and the rebuilt handle is unique from here on).
        let mut cur = match std::mem::replace(&mut p.value, Value::Undefined) {
            Value::Str(s) => s,
            _ => unreachable!("checked above"),
        };
        drop(lval);
        if !cur.append_in_place(x) {
            cur = cur.concat_grown(x);
        }
        p.value = Value::Str(cur);
        Ok(())
    }

    fn try_ic_set(
        &mut self,
        o: &Gc,
        name: &Rc<str>,
        v: &Value,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> bool {
        let mut b = o.borrow_mut();
        if !matches!(b.exotic, Exotic::None) {
            return false;
        }
        if !self.ordinary_get_ptr(Rc::as_ptr(o) as usize) {
            return false;
        }
        let st = cache.get();
        // Shape fast path (own writable data property), both ways: a shape match means `slot`
        // still maps `name`; skip the key compare. `accessor`/`writable` are re-checked (an
        // in-place defineProperty could have flipped them without changing the shape).
        for st in [st, self.ic_way2(cache).get()] {
            if st.depth == 0 && b.props.shape() == st.recv_shape {
                if let Some((_, p)) = b.props.entry_at(st.slot as usize) {
                    if !p.accessor && p.writable {
                        b.props.entry_at_mut(st.slot as usize).unwrap().1.value = v.clone();
                        return true;
                    }
                }
            }
        }
        // Stub-cache probe (shared with the get path — a depth-0 entry means "this shape maps
        // this name at this slot", and accessor/writable are re-checked live either way): a
        // store site rotating through more shapes than its two ways still resolves without the
        // hashed existence scan below. Promote into way 1 like the get path does.
        {
            let shape = b.props.shape();
            let e = self.stub_cache[stub_slot(shape, name)].get();
            if e.name == name.as_ptr() as usize && e.st.recv_shape == shape && e.st.depth == 0 {
                if let Some((_, p)) = b.props.entry_at(e.st.slot as usize) {
                    if !p.accessor && p.writable {
                        b.props.entry_at_mut(e.st.slot as usize).unwrap().1.value = v.clone();
                        self.ic_way2(cache).set(cache.get());
                        cache.set(e.st);
                        return true;
                    }
                }
            }
        }
        let st = cache.get();
        // Creation fast path (see IC_CREATE): the shape match proves `name` is absent, the
        // epoch + prototype identity re-prove the fill-time chain walk, and the recorded child
        // shape makes the insert a plain append — no existence scan, no transition lookup.
        // A saturated epoch (u32::MAX) never validates: past ~4e9 invalidations the creation
        // ICs turn off rather than ABA-cycle.
        if st.depth == crate::bytecode::IC_CREATE
            && st.recv_shape == b.props.shape()
            && st.mid_shape == crate::value::proto_epoch()
            && st.mid_shape != u32::MAX
            && b.extensible
        {
            let proto_ptr = b.proto.as_ref().map_or(0, |p| Rc::as_ptr(p) as usize);
            let key = cache as *const std::cell::Cell<crate::bytecode::IcState> as usize;
            if let Some((pp, _pin)) = self.creation_pins.get(&key) {
                if *pp == proto_ptr {
                    b.props
                        .append_new(name.clone(), Property::plain(v.clone()), st.holder_shape);
                    return true;
                }
            }
        }
        match b.props.slot_of(name) {
            Some(slot) => {
                let p = &b.props.entry_at(slot).unwrap().1;
                if p.accessor || !p.writable {
                    return false; // setter, or non-writable (strict-throw) — slow path
                }
                let shape = b.props.shape();
                b.props.entry_at_mut(slot).unwrap().1.value = v.clone();
                self.ic_way2(cache).set(cache.get());
                let st = crate::bytecode::IcState {
                    depth: 0,
                    slot: slot as u32,
                    recv_shape: shape,
                    holder_shape: shape,
                    mid_ok: 0,
                    mid_shape: 0,
                    mid2_shape: 0,
                };
                cache.set(st);
                // Mirror into the stub cache (see the probe above).
                self.stub_cache[stub_slot(shape, name)].set(StubEntry {
                    name: name.as_ptr() as usize,
                    st,
                });
                true
            }
            None => self.try_ic_create(o, b, name, v, cache),
        }
    }

    /// Property *creation* through the set IC (see [`crate::bytecode::IC_CREATE`]): the hot
    /// constructor pattern `this.x = v` inserts a fresh own property on every call. A cache hit
    /// (same before-shape + same prototype identity + unchanged proto epoch + extensible)
    /// re-proves the fill-time chain walk, so the insert skips `OrdinarySet` entirely. `b` is the
    /// receiver's live borrow (the receiver is already known ordinary, `Exotic::None`).
    fn try_ic_create(
        &mut self,
        _o: &Gc,
        mut b: std::cell::RefMut<'_, crate::value::Object>,
        name: &Rc<str>,
        v: &Value,
        cache: &std::cell::Cell<crate::bytecode::IcState>,
    ) -> bool {
        use crate::bytecode::{IcState, IC_CREATE};
        // Index-like names belong to the dense-element machinery — generic path.
        if name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            return false;
        }
        if !b.extensible {
            return false;
        }
        let st = cache.get();
        let epoch = crate::value::proto_epoch();
        // A saturated epoch can never validate a hit — filling would only waste walks.
        if epoch == u32::MAX {
            return false;
        }
        let proto_ptr = b.proto.as_ref().map_or(0, |p| Rc::as_ptr(p) as usize);
        let key = cache as *const std::cell::Cell<IcState> as usize;
        // (The hit path ran in `try_ic_set` before the existence scan.) A still-valid entry for
        // a DIFFERENT shape marks a polymorphic-creation site (splay rotations assign
        // `left`/`right` on nodes of varying shapes): keep the existing entry — its shape keeps
        // hitting — and send the rest to the generic path instead of paying a re-fill walk per
        // miss.
        if st.depth == IC_CREATE && st.mid_shape == epoch {
            return false;
        }
        // Bound the pin map for cache-churn-heavy embedders (per-request eval): existing sites
        // keep hitting; new sites just stop caching once the budget is spent.
        if self.creation_pins.len() >= 65536 && !self.creation_pins.contains_key(&key) {
            return false;
        }
        // Fill: prove the whole chain clean — every hop a plain ordinary object without an own
        // `name` (which rules out setters and non-writable shadows) — then insert and record.
        // Each hop is marked as a live prototype so any later structural change to it bumps the
        // epoch; proto swaps and defineProperty bump it globally.
        let recv_shape = b.props.shape();
        let mut cur = b.proto.clone();
        {
            let mut walk = cur.clone();
            let mut hops = 0;
            while let Some(h) = walk {
                hops += 1;
                if hops > 8 {
                    return false;
                }
                let hb = h.borrow();
                if !(matches!(hb.exotic, Exotic::None)
                    && self.ordinary_get_ptr(Rc::as_ptr(&h) as usize))
                {
                    return false;
                }
                if hb.props.slot_of(name).is_some() {
                    return false;
                }
                hb.props.mark_proto();
                let next = hb.proto.clone();
                drop(hb);
                walk = next;
            }
        }
        b.props.insert(name.clone(), Property::plain(v.clone()));
        cache.set(IcState {
            depth: IC_CREATE,
            slot: 0,
            recv_shape,
            // The memoized child shape the insert just landed on: hits append straight to it.
            holder_shape: b.props.shape(),
            mid_ok: 0,
            mid_shape: crate::value::proto_epoch(),
            mid2_shape: 0,
        });
        let pin = cur.take().map(|p| Rc::downgrade(&p)).unwrap_or_default();
        self.creation_pins.insert(key, (proto_ptr, pin));
        true
    }

    /// Whether object `ptr` reads/writes like an ordinary object — i.e. it is not registered in any
    /// of the exotic side tables (proxy, module namespace, typed array, deferred namespace). Cheap:
    /// each empty table is skipped without hashing, which is the common case in a hot loop.
    #[inline]
    pub(crate) fn ordinary_get_ptr(&self, ptr: usize) -> bool {
        (self.proxies.is_empty() || !self.proxies.contains_key(&ptr))
            && (self.typed_arrays.is_empty() || !self.typed_arrays.contains_key(&ptr))
            && (self.module_ns.is_empty() || !self.module_ns.contains_key(&ptr))
            && (self.deferred_ns.is_empty() || !self.deferred_ns.contains_key(&ptr))
    }

    /// [[Get]](P, Receiver): like [`get_member`] but with an explicit `receiver` — the `this` a
    /// getter is invoked with, the proxy `get` trap's Receiver argument, and what a forwarded
    /// `[[Get]]` carries through a proxy target chain.
    /// The deferred-namespace evaluation trigger: most operations with a string key (and all
    /// key-less ones like [[OwnPropertyKeys]]) evaluate the module; symbol keys and the "then"
    /// property never do. Accessing a namespace whose module is mid-evaluation is a TypeError.
    /// The proxy record for `ptr`, skipping the table probe entirely when no proxies exist
    /// (every prototype-chain hop asks).
    #[inline(always)]
    pub(crate) fn proxy_at(&self, ptr: usize) -> Option<(Value, Value)> {
        if self.proxies.is_empty() {
            return None;
        }
        self.proxies.get(&ptr).cloned()
    }

    #[inline(always)]
    pub(crate) fn defer_trigger(&mut self, o: &Gc, key: Option<&str>) -> Result<(), Abrupt> {
        if self.deferred_ns.is_empty() {
            return Ok(());
        }
        self.defer_trigger_slow(o, key)
    }

    fn defer_trigger_slow(&mut self, o: &Gc, key: Option<&str>) -> Result<(), Abrupt> {
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

    pub(crate) fn get_member_recv(
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
                // `length` and indices are in UTF-16 code units (cached — a scanner loop reads
                // them per character of a megabytes-long source).
                if key == "length" {
                    let s = s.clone();
                    return Ok(Value::Num(self.str_len(&s) as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    let s = s.clone();
                    return Ok(match self.unit_at(&s, i) {
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
                        .map(|k| Value::Str(k.into()))
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
                        return Ok(Value::Num(self.str_len(&s) as f64));
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        if let Some(u) = self.unit_at(&s, i) {
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
                    if let Some((target, handler)) = self.proxy_at(ptr) {
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
            if let Some((target, handler)) = self.proxy_at(ptr) {
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
            // Clone only the fields the branches read (value / accessor-get), not the whole
            // Property — data reads are the hot path and were paying for three Rc bumps.
            let prop = {
                let b = obj.borrow();
                b.props.get(key).map(|p| {
                    if p.accessor {
                        (true, p.getter().cloned(), Value::Undefined)
                    } else {
                        (false, None, p.value.clone())
                    }
                })
            };
            if let Some((accessor, getter, value)) = prop {
                let p = PropRead {
                    accessor,
                    get: getter,
                    value,
                };
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
                            let plain = match &r.borrow().call {
                                Callable::User(f, _) => {
                                    !f.is_strict
                                        && !f.is_arrow
                                        && !f.is_generator
                                        && !f.is_async
                                        && !f.is_method
                                }
                                _ => false,
                            };
                            if plain {
                                // Legacy SpiderMonkey reflection: the youngest activation's
                                // arguments object / calling function (null when inactive; a
                                // strict caller is censored to null). Eval runs inline, so eval
                                // frames are naturally skipped.
                                let rptr = Rc::as_ptr(r) as usize;
                                let top = self.fn_frames.iter().rposition(|fr| fr.fn_ptr == rptr);
                                return Ok(match (key, top) {
                                    (_, None) => Value::Null,
                                    ("arguments", Some(k)) => {
                                        // The activation skipped building its arguments object
                                        // (the body never names it) — conjure it now.
                                        let lazy = match self.fn_frames[k].extra.as_deref() {
                                            Some(x) if matches!(x.args_obj, Value::Null) => {
                                                x.lazy.clone()
                                            }
                                            _ => None,
                                        };
                                        if let Some((func, args, scope)) = lazy {
                                            let ao =
                                                self.make_arguments_object(&func, &args, &scope, r);
                                            if let Some(x) = self.fn_frames[k].extra.as_deref_mut()
                                            {
                                                x.args_obj = Value::Obj(ao);
                                            }
                                        }
                                        match self.fn_frames[k].extra.as_deref() {
                                            Some(x) => x.args_obj.clone(),
                                            None => Value::Null,
                                        }
                                    }
                                    (_, Some(k)) => match self.fn_frames[..k].last() {
                                        None => Value::Null,
                                        Some(fr) if fr.strict => Value::Null,
                                        Some(fr) => Value::Obj(fr.callee()),
                                    },
                                });
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
    pub(crate) fn set_member_recv(
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
                            return match p.setter().cloned() {
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
            if let Some((target, handler)) = self.proxy_at(ptr) {
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
                    return match p.setter().cloned() {
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
                // OrdinarySetWithOwnDescriptor 4.d: an existing receiver property must be a
                // writable data property (super.x = v / Reflect.set can reach here with a
                // receiver whose own descriptor differs from the base chain's).
                let (accessor, writable) = {
                    let b = obj.borrow();
                    let pr = b.props.get(key).unwrap();
                    (pr.accessor, pr.writable)
                };
                if accessor || !writable {
                    if self.strict {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot assign to read only property '{key}'"),
                        ));
                    }
                    return Ok(false);
                }
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
                // ArraySetLength deletes out-of-range indices from the top down, stopping at a
                // non-configurable one. Equivalent bulk form: everything above the *highest*
                // non-configurable out-of-range index (all necessarily configurable) is removed
                // in one O(n) compaction; length settles just past the blocker (strict throws).
                let blocker: Option<usize> = obj
                    .borrow()
                    .props
                    .iter()
                    .filter(|(_, p)| !p.configurable)
                    .filter_map(|(k, _)| {
                        // Only array-index keys (canonical, < 2^32-1) participate in truncation;
                        // "4294967296" and friends are ordinary properties.
                        crate::value::canonical_index(k).map(|n| n as usize)
                    })
                    .filter(|&idx| idx >= new_len)
                    .max();
                match blocker {
                    None => obj.borrow_mut().props.remove_indices_from(new_len),
                    Some(b) => {
                        let mut ob = obj.borrow_mut();
                        ob.props.remove_indices_from(b + 1);
                        ob.props.insert(
                            "length",
                            Property::data(Value::Num((b + 1) as f64), true, false, false),
                        );
                        drop(ob);
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
    pub(crate) fn ta_len(&self, info: &TaInfo) -> Option<usize> {
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
    pub(crate) fn ta_index_kind(&self, info: &TaInfo, key: &str) -> TaIndex {
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
    pub(crate) fn canonical_numeric_index(&self, key: &str) -> Option<f64> {
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

    pub(crate) fn array_length(&self, obj: &Gc) -> usize {
        // A TypedArray's length lives in its info slot, not an own `length` property.
        if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(obj) as usize)) {
            return self.ta_len(info).unwrap_or(0);
        }
        match obj.borrow().props.length_property().map(|p| p.value.clone()) {
            Some(Value::Num(n)) => n as usize,
            _ => 0,
        }
    }

    /// Array length for an operation that will iterate/allocate proportional to it. Errors with a
    /// RangeError past [`MAX_ARRAY_OP_LEN`] so a huge `.length` cannot exhaust memory.
    pub(crate) fn checked_array_len(&mut self, obj: &Gc) -> Result<usize, Abrupt> {
        let len = self.to_length(obj)?;
        if len > MAX_ARRAY_OP_LEN {
            return Err(self.throw("RangeError", "array length exceeds engine limit"));
        }
        Ok(len)
    }

    /// ToLength of an array-like's `length` property (coercing string/object lengths), clamped to
    /// the 2^53-1 spec maximum.
    pub(crate) fn to_length(&mut self, obj: &Gc) -> Result<usize, Abrupt> {
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
    /// The cached UTF-16 view of `s` (see [`StrUnits`]): first access per string is O(len),
    /// every later one O(1) — pointer-compared against a small LRU. Short strings skip the
    /// cache entirely (the O(len) walk is trivial; caching them would just churn the LRU under
    /// code that touches thousands of small strings once each).
    pub(crate) fn units_of(&mut self, s: &crate::lstr::LStr) -> StrUnits {
        if s.len() < 64 {
            return if s.is_ascii() {
                StrUnits::Ascii
            } else {
                StrUnits::Units(crate::jstr::units(s).into())
            };
        }
        if let Some(k) = self
            .str_units
            .iter()
            .position(|(k, _)| crate::lstr::LStr::ptr_eq(k, s))
        {
            let hit = self.str_units[k].1.clone();
            // Keep the hot entry last (evictions pop from the front).
            let n = self.str_units.len();
            self.str_units.swap(k, n - 1);
            return hit;
        }
        let u = if s.is_ascii() {
            StrUnits::Ascii
        } else {
            StrUnits::Units(crate::jstr::units(s).into())
        };
        if self.str_units.len() >= 8 {
            self.str_units.remove(0);
        }
        self.str_units.push((s.clone(), u.clone()));
        u
    }

    /// Code unit `idx` of `s`, through the cache. `None` = out of range.
    pub(crate) fn unit_at(&mut self, s: &crate::lstr::LStr, idx: usize) -> Option<u16> {
        match self.units_of(s) {
            StrUnits::Ascii => s.as_bytes().get(idx).map(|&b| b as u16),
            StrUnits::Units(u) => u.get(idx).copied(),
        }
    }

    /// The fully materialized unit vector (for range/search operations): an ASCII entry is
    /// promoted in place, so the copy happens once per string, not per call.
    pub(crate) fn units_full(&mut self, s: &crate::lstr::LStr) -> Rc<[u16]> {
        if s.len() < 64 {
            return crate::jstr::units(s).into();
        }
        if let Some(k) = self
            .str_units
            .iter()
            .position(|(k, _)| crate::lstr::LStr::ptr_eq(k, s))
        {
            if let StrUnits::Units(u) = &self.str_units[k].1 {
                return u.clone();
            }
            let u: Rc<[u16]> = crate::jstr::units(s).into();
            self.str_units[k].1 = StrUnits::Units(u.clone());
            return u;
        }
        let u: Rc<[u16]> = crate::jstr::units(s).into();
        if self.str_units.len() >= 8 {
            self.str_units.remove(0);
        }
        self.str_units.push((s.clone(), StrUnits::Units(u.clone())));
        u
    }

    /// `s.length` (UTF-16 units), through the cache.
    pub(crate) fn str_len(&mut self, s: &crate::lstr::LStr) -> usize {
        match self.units_of(s) {
            StrUnits::Ascii => s.len(),
            StrUnits::Units(u) => u.len(),
        }
    }

    /// [`gc_check`] amortized for the JIT fast call: the real check reads two thread-local
    /// counters, which is measurable at millions of calls per second — poll every 16th call.
    #[inline]
    pub(crate) fn gc_check_amortized(&mut self) -> Result<(), Abrupt> {
        self.gc_tick = self.gc_tick.wrapping_add(1);
        if self.gc_tick & 15 == 0 {
            self.gc_check()
        } else {
            Ok(())
        }
    }

    pub(crate) fn gc_check(&mut self) -> Result<(), Abrupt> {
        // Scope churn is tracked separately: call-heavy code can retire millions of scopes while
        // allocating few objects, and each dead weak registry entry pins its allocation.
        if scope_registry_len() > self.scope_gc_next {
            let live = scope_registry_prune();
            self.scope_gc_next = live.saturating_mul(2).max(SCOPE_GC_TRIGGER);
        }
        if crate::value::live_objects() <= self.gc_next {
            return Ok(());
        }
        self.gc_collect();
        let live = crate::value::live_objects();
        if std::env::var_os("LUMEN_GC_LOG").is_some() {
            eprintln!("[gc] live={live}");
        }
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

    /// A regex-ready view of `s`, cached by string identity: repeated exec/replace/split over the
    /// same subject (the common shape of both real code and the regexp benchmarks) skips the
    /// O(len) element-vector rebuild. Strings are immutable, and each entry holds its `Rc` so the
    /// pointer can't be reused by a new allocation while cached.
    pub(crate) fn re_text(
        &mut self,
        unicode: bool,
        s: &crate::lstr::LStr,
    ) -> Rc<crate::regex::ReText> {
        let key = s.as_ptr();
        if let Some((_, _, t)) = self
            .re_texts
            .iter()
            .find(|(k, u, _)| k.as_ptr() == key && *u == unicode)
        {
            return t.clone();
        }
        let t = Rc::new(crate::regex::ReText::new_rc(unicode, s));
        if self.re_texts.len() >= 4 {
            self.re_texts.remove(0);
        }
        self.re_texts.push((s.clone(), unicode, t.clone()));
        t
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
            if let Some(Value::Obj(p)) = prop.getter() {
                refs.push(p.clone());
            }
            if let Some(Value::Obj(p)) = prop.setter() {
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
    /// The scopes `o` refers to: a user function's closure environment, a mapped `arguments`
    /// object's aliased parameter scope, and a class constructor's field-initializer environment.
    fn obj_scope_refs(&self, o: &Gc) -> Vec<Env> {
        let mut out = Vec::new();
        if let Callable::User(_, env) = &o.borrow().call {
            out.push(env.clone());
        }
        let ptr = Rc::as_ptr(o) as usize;
        if let Some((env, _)) = self.mapped_arguments.get(&ptr) {
            out.push(env.clone());
        }
        if let Some(ci) = self.class_info.get(&ptr) {
            out.push(ci.field_env.clone());
        }
        out
    }

    pub(crate) fn gc_collect(&mut self) {
        let live = crate::value::gc_snapshot();
        // Scopes are graph nodes too: a closure's captured environment references objects (its
        // bindings) and vice versa (`Callable::User`), so cycles routinely pass through them.
        let scopes = scope_snapshot();
        let sidx: crate::fasthash::FastMap<usize, usize> = scopes
            .iter()
            .enumerate()
            .map(|(k, e)| (Rc::as_ptr(e) as usize, k))
            .collect();
        let mut s_internal = vec![0u32; scopes.len()];
        let mut s_mark = vec![false; scopes.len()];

        // Reset scratch, then count references between heap nodes (objects and scopes).
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
            for e in self.obj_scope_refs(o) {
                if let Some(&k) = sidx.get(&(Rc::as_ptr(&e) as usize)) {
                    s_internal[k] += 1;
                }
            }
        }
        for e in &scopes {
            let b = e.borrow();
            if let Some(p) = &b.parent {
                if let Some(&k) = sidx.get(&(Rc::as_ptr(p) as usize)) {
                    s_internal[k] += 1;
                }
            }
            if let Some(Value::Obj(o)) = &b.with_obj {
                let ob = o.borrow();
                ob.gc_internal.set(ob.gc_internal.get() + 1);
            }
            for bind in b.vars.values() {
                if let Value::Obj(o) = &bind.value {
                    let ob = o.borrow();
                    ob.gc_internal.set(ob.gc_internal.get() + 1);
                }
                if let Some((ie, _)) = &bind.import_ref {
                    if let Some(&k) = sidx.get(&(Rc::as_ptr(ie) as usize)) {
                        s_internal[k] += 1;
                    }
                }
            }
        }

        // A pin is bookkeeping, not a real holder: count it like an internal reference so a
        // pinned-but-unreachable object is still collectable (the sweep evicts its entries).
        for o in self.gc_pins.values() {
            let b = o.borrow();
            b.gc_internal.set(b.gc_internal.get() + 1);
        }
        // Roots: nodes with a reference from outside the heap graph (the Rust call stack, the
        // Interp's own fields, module/realm registries, coroutine threads). `strong_count`
        // includes exactly one clone held by the snapshot, so external refs == strong - internal - 1.
        let mut stack: Vec<Gc> = Vec::new();
        let mut sstack: Vec<Env> = Vec::new();
        for o in &live {
            let internal = o.borrow().gc_internal.get() as usize;
            if Rc::strong_count(o) > internal + 1 {
                o.borrow().gc_mark.set(true);
                stack.push(o.clone());
            }
        }
        for (k, e) in scopes.iter().enumerate() {
            if Rc::strong_count(e) > s_internal[k] as usize + 1 {
                s_mark[k] = true;
                sstack.push(e.clone());
            }
        }
        // Mark everything reachable from the roots, across both node types.
        loop {
            if let Some(o) = stack.pop() {
                for p in Self::obj_refs(&o) {
                    if !p.borrow().gc_mark.get() {
                        p.borrow().gc_mark.set(true);
                        stack.push(p);
                    }
                }
                for e in self.obj_scope_refs(&o) {
                    if let Some(&k) = sidx.get(&(Rc::as_ptr(&e) as usize)) {
                        if !s_mark[k] {
                            s_mark[k] = true;
                            sstack.push(e);
                        }
                    }
                }
                continue;
            }
            let Some(e) = sstack.pop() else { break };
            let b = e.borrow();
            if let Some(p) = &b.parent {
                if let Some(&k) = sidx.get(&(Rc::as_ptr(p) as usize)) {
                    if !s_mark[k] {
                        s_mark[k] = true;
                        sstack.push(p.clone());
                    }
                }
            }
            if let Some(Value::Obj(o)) = &b.with_obj {
                if !o.borrow().gc_mark.get() {
                    o.borrow().gc_mark.set(true);
                    stack.push(o.clone());
                }
            }
            for bind in b.vars.values() {
                if let Value::Obj(o) = &bind.value {
                    if !o.borrow().gc_mark.get() {
                        o.borrow().gc_mark.set(true);
                        stack.push(o.clone());
                    }
                }
                if let Some((ie, _)) = &bind.import_ref {
                    if let Some(&k) = sidx.get(&(Rc::as_ptr(ie) as usize)) {
                        if !s_mark[k] {
                            s_mark[k] = true;
                            sstack.push(ie.clone());
                        }
                    }
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
        // Sweep garbage scopes the same way: emptying them breaks env-involving cycles.
        for (k, e) in scopes.iter().enumerate() {
            if !s_mark[k] {
                let mut b = e.borrow_mut();
                b.vars.clear();
                b.parent = None;
                b.with_obj = None;
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
        if let Err(e) = self.gc_check_amortized() {
            self.depth -= 1;
            return Err(e);
        }
        let mut r = self.call_inner(callee, this, args);
        // Trampoline: a proper tail call unwound out of the callee re-dispatches here, in the
        // same stack frame, so mutual tail recursion runs in constant stack space.
        while r.is_ok() {
            match self.pending_tail.take() {
                Some(bx) => {
                    let (f, t, a) = *bx;
                    if let Err(e) = self.gc_check_amortized() {
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

    pub(crate) fn call_inner(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Abrupt> {
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
            // Walk to the scope root by raw pointer — every hop is kept alive transitively by
            // `env` (which the callee's `call` field owns) and nothing mutates the parent links,
            // so this skips a refcount round-trip per hop on every construct/call that asks.
            let root = {
                let mut cur: *const RefCell<Scope> = Rc::as_ptr(env);
                loop {
                    let parent = unsafe { (*cur).borrow().parent.as_ref().map(Rc::as_ptr) };
                    match parent {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                cur
            };
            // The overwhelmingly common case: the callee closes over THIS realm's global scope.
            if root == Rc::as_ptr(&self.global_env) {
                return None;
            }
            for (g, rs) in &self.realms {
                if root == Rc::as_ptr(&rs.global_env) {
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
        // `eval(...)` of the current realm is intercepted earlier, in `eval_call`.) Only a
        // native function can be a realm's `eval` — skip the property lookup for user calls,
        // which is every hot call (the main realm always registers, so the map is never empty).
        if !self.realms.is_empty()
            && self.eval_realm_fns.contains(&(Rc::as_ptr(&obj) as usize))
        {
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
            Callable::Native(_) | Callable::NativeData(_) => self
                .dispatch_native(&call, this, args)
                .map_err(Abrupt::Throw),
            Callable::User(func, env) => {
                // A class constructor cannot be [[Call]]ed. (Empty-map guard: this runs on every
                // single call, and most programs define no classes.)
                if !self.class_info.is_empty()
                    && self.class_info.contains_key(&(Rc::as_ptr(&obj) as usize))
                {
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
                                Callable::Native(_) | Callable::NativeData(_) => "native",
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

    pub(crate) fn make_wrapped_shadow(
        &mut self,
        realm: usize,
        target: Value,
    ) -> Result<Value, Abrupt> {
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

    /// Build the `arguments` exotic object for an activation: indexed own props + configurable
    /// `length`, `@@iterator` = Array.prototype.values. An unmapped one (strict function OR
    /// non-simple parameter list) exposes `callee` as the %ThrowTypeError% poison accessor; a
    /// mapped one aliases still-mapped indices to the parameter bindings in `scope` (see
    /// `mapped_arguments`) and carries the function as `callee`.
    pub(crate) fn make_arguments_object(
        &mut self,
        func: &Rc<Function>,
        args: &[Value],
        scope: &Env,
        fn_obj: &Gc,
    ) -> Gc {
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
                    crate::value::Property::accessor_prop(
                        Some(Value::Obj(tte.clone())),
                        Some(Value::Obj(tte)),
                        false,
                        false,
                    ),
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
        ao
    }

    /// OrdinaryCallBindThis for a compiled body, computed only when the body reads `this`. A
    /// construct's `this` is the fresh instance — bound directly, never coerced; a sloppy plain
    /// call boxes a primitive / substitutes the global for undefined/null.
    fn bind_compiled_this(
        &mut self,
        func: &Rc<Function>,
        chunk: &crate::bytecode::Chunk,
        this: Value,
        is_construct: bool,
    ) -> Value {
        if !chunk.uses_this() {
            return Value::Undefined;
        }
        if func.is_strict || is_construct {
            return this;
        }
        match this {
            Value::Undefined | Value::Null => Value::Obj(self.global.clone()),
            other @ Value::Obj(_) => other,
            prim => crate::builtins::box_primitive_pub(self, prim),
        }
    }

    /// Run an already-compiled chunk (JIT or bytecode VM), saving/restoring the per-body engine
    /// flags (`strict`, `tco_ok`, field-init / async-gen-body markers). Shared by the lean
    /// [`call_user_inner`] path and the [`call_compiled_fast`] shortcut so they can't diverge.
    /// `this_val` is already OrdinaryCallBindThis-bound; `new_target`/`constructing`/frame
    /// bookkeeping is the caller's job.
    fn run_compiled_chunk(
        &mut self,
        func: &Rc<Function>,
        chunk: &Rc<crate::bytecode::Chunk>,
        closure: &Env,
        this_val: Value,
        args: &[Value],
        is_construct: bool,
    ) -> Result<Value, Abrupt> {
        let saved_strict = std::mem::replace(&mut self.strict, func.is_strict);
        let saved_tco = std::mem::replace(&mut self.tco_ok, func.is_strict && !is_construct);
        let saved_field_init = self.in_field_init_code;
        let saved_agb = self.in_async_gen_body;
        if !func.is_arrow {
            self.in_field_init_code = false;
            self.in_async_gen_body = false;
        }
        // Machine-code tier: compile once (None = unsupported — async or platform), then run the
        // JIT body; otherwise the bytecode VM.
        let mut jit_code = None;
        if matches!(self.tier, crate::bytecode::Tier::Jit) {
            if chunk.jit.get().is_none() {
                let layout = *self
                    .jit_layout
                    .get_or_init(|| crate::value::jit_layout(&self.object_proto));
                if !self.interp_layout.get().valid {
                    let l = interp_layout(self);
                    self.interp_layout.set(l);
                }
                let _ = chunk
                    .jit
                    .set(crate::jit::compile(chunk, &layout).map(std::rc::Rc::new));
            }
            if let Some(Some(code)) = chunk.jit.get() {
                jit_code = Some(code.clone());
            }
        }
        let r = match jit_code {
            Some(code) => crate::jit::run(self, chunk, &code, closure, this_val, args),
            None => crate::bytecode::run(self, chunk, closure, this_val, args),
        };
        self.strict = saved_strict;
        self.tco_ok = saved_tco;
        self.in_field_init_code = saved_field_init;
        self.in_async_gen_body = saved_agb;
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
        self.fn_frames.push(FnFrame {
            fn_ptr: Rc::as_ptr(fn_obj) as usize,
            coro: self.cur_coro,
            strict: func.is_strict,
            extra: None,
        });
        let r = self.call_user_inner(func, closure, this, args, is_construct, fn_obj);
        self.fn_frames.pop();
        r
    }

    /// The JIT→JIT fast call (from `bytecode::jit_exec`'s Call/CallWithThis arms): a plain,
    /// same-realm, already-JIT-compiled user function with no activation environment runs with
    /// exactly the observable effects of the layered path — recursion depth, gc_check, the
    /// `FnFrame` for `f.caller` reflection, constructing/new.target save-clear-restore, and the
    /// proper-tail-call trampoline — but skips the dispatch layers' proxy/realm/eval-marker
    /// re-checks (guarded up front) and *moves* the `argc` argument `Value`s at `args` into the
    /// callee's slots instead of clone-here-drop-there.
    ///
    /// `None` = not applicable, with NO side effects and the arguments untouched (the caller
    /// runs the generic path). `Some(r)` = handled; the arguments AND `*this_slot` have been
    /// consumed (the `this` binding moves into the callee instead of a clone-here-drop-there).
    ///
    /// The identity-cached JIT→JIT call (see [`crate::bytecode::CallIc`]): on a per-site hit,
    /// the entire dispatch guard set collapses into two pointer compares (callee identity +
    /// active-realm global), and the frame's derived state (env, chunk, machine code, strict)
    /// reads through the cached raw pointers with a single refcount bump for the env handle.
    /// `None` = miss (empty cache / different callee / realm switched), with NO side effects —
    /// the caller falls into [`Interp::call_jit_fast`], which revalidates and refills.
    ///
    /// # Safety
    /// Same contract as `call_jit_fast`: on `Some`, `args..args+argc` and `*this_slot` have been
    /// consumed.
    pub(crate) unsafe fn call_jit_cached(
        &mut self,
        site: &crate::bytecode::CallSite,
        callee: &Value,
        this_slot: *const Value,
        args: *mut Value,
        argc: usize,
    ) -> Option<Result<Value, Abrupt>> {
        let Value::Obj(o) = callee else { return None };
        let key = Rc::as_ptr(o) as usize;
        let genv = Rc::as_ptr(&self.global_env) as usize;
        // Probe the identity fields through the Cell without copying whole entries; only the
        // hit is copied out (nothing re-entrant runs between the probe and the copy).
        let epoch = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        let mut hit = None;
        for e in &site.entries {
            let p = e.as_ptr();
            unsafe {
                if (*p).callee == key && (*p).global_env == genv && (*p).epoch == epoch {
                    hit = Some(*p);
                    break;
                }
            }
        }
        let ic = hit?;
        // Inline-recompile trigger: a chunk that keeps running in machine code gets one shot at
        // splicing its own hot monomorphic callees (see `bytecode::plan_inlines`).
        {
            let chunk_ref = unsafe { &**ic.chunk };
            let runs = chunk_ref.jit_runs.get().wrapping_add(1);
            chunk_ref.jit_runs.set(runs);
            if runs == crate::bytecode::inline_recompile_at() {
                self.try_inline_recompile(ic.func, chunk_ref);
            }
        }
        Some(unsafe { self.call_jit_committed(ic, this_slot, args, argc) })
    }

    /// [`Interp::call_jit_cached`]'s committed tail, split out so the asm call thunk can enter
    /// it with an already-validated `CallIc` (the probe having run in machine code). From here
    /// the arguments and `*this_slot` are consumed unconditionally.
    ///
    /// # Safety
    /// `ic` must be a hit validated THIS turn (callee identity + epoch + realm), with the callee
    /// object alive in the caller's hands; `args..args+argc` and `*this_slot` are live
    /// operand-stack values the caller forgets.
    pub(crate) unsafe fn call_jit_committed(
        &mut self,
        ic: crate::bytecode::CallIc,
        this_slot: *const Value,
        args: *mut Value,
        argc: usize,
    ) -> Result<Value, Abrupt> {
        // --- committed: identical to call_jit_fast's committed path ---
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
                std::ptr::drop_in_place(this_slot as *mut Value);
            }
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        if let Err(e) = self.gc_check_amortized() {
            self.depth -= 1;
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
                std::ptr::drop_in_place(this_slot as *mut Value);
            }
            return Err(e);
        }
        let saved_ctor = std::mem::replace(&mut self.constructing, false);
        let saved_nt = std::mem::replace(&mut self.new_target, Value::Undefined);
        self.fn_frames.push(FnFrame {
            fn_ptr: ic.callee,
            coro: self.cur_coro,
            strict: ic.strict,
            extra: None,
        });
        // Borrow the env without touching its refcount: it stays alive for the whole call
        // because the callee object (held by the caller's operand stack) owns it through its
        // `call` field, which is never reassigned while live. ManuallyDrop = a non-owning
        // `&Env` fabricated from the raw pointer; nothing below moves it out.
        let env = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(ic.env) });
        let chunk = unsafe { &*ic.chunk };
        let code = unsafe { &*ic.code };
        // OrdinaryCallBindThis on the moved `this` (bind_compiled_this, inlined over `ic.strict`
        // and the fill-time `uses_this`).
        let this_read = unsafe { this_slot.read() };
        let this_val = if !ic.uses_this {
            drop(this_read);
            Value::Undefined
        } else if ic.strict {
            this_read
        } else {
            match this_read {
                Value::Undefined | Value::Null => Value::Obj(self.global.clone()),
                other @ Value::Obj(_) => other,
                prim => crate::builtins::box_primitive_pub(self, prim),
            }
        };
        let mut r = unsafe {
            crate::jit::run_moved(
                self,
                chunk,
                code,
                &*env as *const Env,
                this_val,
                args,
                argc,
                (ic.n_params as usize, ic.n_slots as usize),
            )
        };
        self.fn_frames.pop();
        self.constructing = saved_ctor;
        self.new_target = saved_nt;
        // Proper-tail-call trampoline, exactly like `call`.
        while r.is_ok() {
            match self.pending_tail.take() {
                Some(bx) => {
                    let (f, t, a) = *bx;
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

    /// One-shot second-stage compile of a hot chunk's function with its monomorphic callees
    /// spliced inline. On success the new chunk lands in `Function::code2` and the global call-IC
    /// epoch bumps, so every cached caller re-resolves and picks it up; the old chunk stays
    /// alive (and correct) beneath any pointers already handed out.
    pub(crate) fn try_inline_recompile(
        &mut self,
        func: *const crate::ast::Function,
        chunk: &crate::bytecode::Chunk,
    ) {
        if func.is_null() || chunk.inline_attempted.replace(true) {
            return;
        }
        // Alive because the callee object that carried this IC is alive and its `call` field is
        // never reassigned (the same argument that makes the rest of the IC readable).
        let func = unsafe { &*func };
        if func.code2.get().is_some() {
            return;
        }
        let global_env = self.global_env.clone();
        let plan = crate::bytecode::plan_inlines(chunk, func, &global_env);
        if plan.is_empty() {
            if std::env::var_os("LUMEN_TIER_LOG").is_some() {
                let src = func.source.as_deref().unwrap_or("<no source>");
                let head: String = src.chars().take(60).collect();
                eprintln!("[tier] inline: empty plan for: {}", head.replace('\n', " "));
            }
            return;
        }
        if let Some(chunk2) = crate::bytecode::compile_with_inlines(func, &plan) {
            if std::env::var_os("LUMEN_TIER_LOG").is_some() {
                let src = func.source.as_deref().unwrap_or("<no source>");
                let head: String = src.chars().take(60).collect();
                eprintln!(
                    "[tier] inlined {} site(s) into: {}",
                    plan.len(),
                    head.replace('\n', " ")
                );
            }
            let _ = func.code2.set(Some(chunk2));
            crate::bytecode::CALL_IC_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// # Safety
    /// `args..args+argc` and `*this_slot` must be initialized `Value`s the caller relinquishes
    /// on `Some`.
    /// The identity-cached JIT construct (`Op::New` with a plain, same-realm, compiled,
    /// no-activation function constructor): replicates `construct` → `construct_dispatch`'s
    /// observable effects for that case — IsConstructor, prototype read (LIVE, each time:
    /// `F.prototype = other` is a plain value write no epoch sees), OrdinaryCreateFromConstructor,
    /// depth guard, gc tick, `FnFrame`, new.target = the callee for the body's duration,
    /// return-override — while skipping the dispatch layering, and MOVES the argument values
    /// off the caller's operand stack like the cached call does.
    ///
    /// `None` = not applicable, no side effects, arguments untouched (caller runs the generic
    /// path). `Some(r)` = handled; the `argc` arguments at `args` were consumed.
    ///
    /// # Safety
    /// Same contract as `call_jit_cached`: `args..args+argc` must be live operand-stack values
    /// the caller forgets on `Some`.
    pub(crate) unsafe fn construct_jit_fast(
        &mut self,
        callee: &Value,
        args: *mut Value,
        argc: usize,
    ) -> Option<Result<Value, Abrupt>> {
        if !self.proxies.is_empty() {
            return None;
        }
        let Value::Obj(o) = callee else { return None };
        let key = Rc::as_ptr(o) as usize;
        let epoch = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        let genv = Rc::as_ptr(&self.global_env) as usize;
        let ic = match self.construct_ics.get(&key) {
            Some((ic, _)) if ic.epoch == epoch && ic.global_env == genv => *ic,
            _ => self.construct_ic_fill(o, key, epoch, genv)?,
        };
        // OrdinaryCreateFromConstructor: the instance prototype is the constructor's LIVE
        // `prototype` own data property; anything else (accessor, non-object, absent) → generic.
        let proto = {
            let b = o.borrow();
            match b.props.get("prototype") {
                Some(p) if !p.accessor => match &p.value {
                    Value::Obj(pp) => pp.clone(),
                    _ => return None,
                },
                _ => return None,
            }
        };
        let this = crate::value::Object::new(Some(proto));
        let this_val = Value::Obj(this);
        // --- committed: identical shape to call_jit_cached's committed path ---
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
            }
            return Some(Err(
                self.throw("RangeError", "Maximum call stack size exceeded")
            ));
        }
        if let Err(e) = self.gc_check_amortized() {
            self.depth -= 1;
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
            }
            return Some(Err(e));
        }
        let saved_ctor = std::mem::replace(&mut self.constructing, false);
        // The body runs with new.target = the callee (what the layered path's pending-new-target
        // handoff produces for `new F()`); compiled chunks can't read it directly, but natives
        // the body invokes could observe it ambiently.
        let saved_nt = std::mem::replace(&mut self.new_target, callee.clone());
        self.fn_frames.push(FnFrame {
            fn_ptr: key,
            coro: self.cur_coro,
            strict: ic.strict,
            extra: None,
        });
        let env = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(ic.env) });
        let chunk = unsafe { &*ic.chunk };
        let code = unsafe { &*ic.code };
        let tv = if ic.uses_this {
            this_val.clone()
        } else {
            Value::Undefined
        };
        let mut r = unsafe {
            crate::jit::run_moved(
                self,
                chunk,
                code,
                &*env as *const Env,
                tv,
                args,
                argc,
                (ic.n_params as usize, ic.n_slots as usize),
            )
        };
        self.fn_frames.pop();
        self.constructing = saved_ctor;
        self.new_target = saved_nt;
        while r.is_ok() {
            match self.pending_tail.take() {
                Some(bx) => {
                    let (f, t, a) = *bx;
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
        // A constructor explicitly returning an object overrides the instance.
        Some(r.map(|ret| match ret {
            v @ Value::Obj(_) => v,
            _ => this_val,
        }))
    }

    /// [`Interp::construct_jit_fast`]'s miss path: revalidate the full guard set (the same one
    /// `call_jit_fast` uses, plus [[Construct]]'s own arrow/method/generator/async and
    /// class-constructor exclusions) and cache the derived state under the callee's identity.
    fn construct_ic_fill(
        &mut self,
        o: &Gc,
        key: usize,
        epoch: u32,
        genv: usize,
    ) -> Option<crate::bytecode::CallIc> {
        let (func, env) = match &o.borrow().call {
            Callable::User(f, e) => (f.clone(), e.clone()),
            _ => return None,
        };
        if func.is_arrow || func.is_method || func.is_generator || func.is_async {
            return None;
        }
        if !self.realms.is_empty() {
            type ScopeCell = RefCell<Scope>;
            let mut cur: *const ScopeCell = Rc::as_ptr(&env);
            let root_is_active = 'walk: {
                for _ in 0..8 {
                    if cur == Rc::as_ptr(&self.global_env) {
                        break 'walk true;
                    }
                    match unsafe { (*cur).borrow().parent.as_ref().map(Rc::as_ptr) } {
                        Some(p) => cur = p,
                        None => break 'walk false,
                    }
                }
                false
            };
            if !root_is_active {
                return None;
            }
        }
        if !self.class_info.is_empty() && self.class_info.contains_key(&key) {
            return None;
        }
        let chunk = match func.code2.get().or_else(|| func.code.get()) {
            Some(Some(c)) => c,
            _ => return None,
        };
        let code = match chunk.jit.get() {
            Some(Some(c)) => c,
            _ => return None,
        };
        if !chunk.jit_no_activation() {
            return None;
        }
        let (n_params, n_slots) = chunk.jit_frame();
        if n_params > u16::MAX as usize || n_slots > u16::MAX as usize {
            return None;
        }
        if self.construct_ics.len() >= 65536 {
            return None; // runaway-ctor backstop; identity caching should never get here
        }
        if !self
            .global_env_pins
            .iter()
            .any(|g| Rc::ptr_eq(g, &self.global_env))
        {
            let g = self.global_env.clone();
            self.global_env_pins.push(g);
        }
        let ic = crate::bytecode::CallIc {
            callee: key,
            env: Rc::as_ptr(&env),
            chunk: chunk as *const Rc<crate::bytecode::Chunk>,
            code: Rc::as_ptr(code),
            global_env: genv,
            strict: func.is_strict,
            uses_this: chunk.uses_this(),
            n_params: n_params as u16,
            n_slots: n_slots as u16,
            func: Rc::as_ptr(&func),
            epoch,
        };
        self.construct_ics.insert(key, (ic, Rc::downgrade(o)));
        Some(ic)
    }

    pub(crate) unsafe fn call_jit_fast(
        &mut self,
        callee: &Value,
        this_slot: *const Value,
        args: *mut Value,
        argc: usize,
        // The caller's per-site cache + pin map: filled once the full guard set passes, so
        // subsequent calls take [`Interp::call_jit_cached`]'s pointer-compare path.
        site: Option<(
            &crate::bytecode::CallSite,
            &RefCell<crate::fasthash::FastMap<usize, std::rc::Weak<RefCell<crate::value::Object>>>>,
        )>,
    ) -> Option<Result<Value, Abrupt>> {
        // Any exotic engine state (live proxies, multiple realms with possible cross-realm
        // callees, legacy fn.caller hooks) takes the generic path. Realms: the callee is
        // same-realm iff its scope chain roots in the active global env — walk a few hops by
        // raw pointer (no Rc churn; the chain is kept alive by the callee's env handle).
        if !self.proxies.is_empty() {
            return None;
        }
        let Value::Obj(o) = callee else { return None };
        let (func, env) = match &o.borrow().call {
            Callable::User(f, e) => (f.clone(), e.clone()),
            _ => return None,
        };
        // Arrows inherit new.target lexically (the generic path skips the clear for them).
        if func.is_arrow {
            return None;
        }
        if !self.realms.is_empty() {
            type ScopeCell = RefCell<Scope>;
            let mut cur: *const ScopeCell = Rc::as_ptr(&env);
            let root_is_active = 'walk: {
                for _ in 0..8 {
                    if cur == Rc::as_ptr(&self.global_env) {
                        break 'walk true;
                    }
                    match unsafe { (*cur).borrow().parent.as_ref().map(Rc::as_ptr) } {
                        Some(p) => cur = p,
                        None => break 'walk false,
                    }
                }
                false
            };
            if !root_is_active {
                return None;
            }
        }
        if !self.class_info.is_empty() && self.class_info.contains_key(&(Rc::as_ptr(o) as usize)) {
            return None;
        }
        // Not yet tiered / didn't compile / no machine code → generic (which counts calls up).
        // The chunk and code stay borrowed through `func` (both OnceCells are set exactly once,
        // and the held `func` clone keeps them alive across the run) — no Rc round-trips.
        let chunk = match func.code2.get().or_else(|| func.code.get()) {
            Some(Some(c)) => c,
            _ => return None,
        };
        let code = match chunk.jit.get() {
            Some(Some(c)) => c,
            _ => return None,
        };
        if !chunk.jit_no_activation() {
            return None;
        }
        // Every guard passed: remember the callee at this site so the next call takes the
        // one-compare cached path. The Weak pin makes the raw identity compare ABA-safe (the
        // address can't be recycled while pinned); a megamorphic site stops refilling at the cap
        // instead of growing the pin list without bound.
        if let Some((cs, pins)) = site {
            let key = Rc::as_ptr(o) as usize;
            let mut p = pins.borrow_mut();
            if p.len() < 4096 || p.contains_key(&key) {
                p.entry(key).or_insert_with(|| Rc::downgrade(o));
                // Pin the active global scope's address too: the cached same-realm proof
                // compares it raw, so it must never be recycled while cached (realms already
                // outlive their caches in practice; this makes it airtight). Fills are rare, so
                // the linear dedup scan (bounded by realm count) doesn't matter.
                if !self
                    .global_env_pins
                    .iter()
                    .any(|g| Rc::ptr_eq(g, &self.global_env))
                {
                    let genv = self.global_env.clone();
                    self.global_env_pins.push(genv);
                }
                let (n_params, n_slots) = chunk.jit_frame();
                if n_params <= u16::MAX as usize && n_slots <= u16::MAX as usize {
                    cs.fill(crate::bytecode::CallIc {
                        callee: key,
                        env: Rc::as_ptr(&env),
                        chunk: chunk as *const Rc<crate::bytecode::Chunk>,
                        code: Rc::as_ptr(code),
                        global_env: Rc::as_ptr(&self.global_env) as usize,
                        strict: func.is_strict,
                        uses_this: chunk.uses_this(),
                        n_params: n_params as u16,
                        n_slots: n_slots as u16,
                        func: Rc::as_ptr(&func),
                        epoch: crate::bytecode::CALL_IC_EPOCH
                            .load(std::sync::atomic::Ordering::Relaxed),
                    });
                }
            }
        }
        // --- committed: from here the arguments and `*this_slot` are ours ---
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            // Ownership contract: consume the arguments and `this` even on the early throw.
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
                std::ptr::drop_in_place(this_slot as *mut Value);
            }
            return Some(Err(
                self.throw("RangeError", "Maximum call stack size exceeded")
            ));
        }
        if let Err(e) = self.gc_check_amortized() {
            self.depth -= 1;
            unsafe {
                for k in 0..argc {
                    std::ptr::drop_in_place(args.add(k));
                }
                std::ptr::drop_in_place(this_slot as *mut Value);
            }
            return Some(Err(e));
        }
        let saved_ctor = std::mem::replace(&mut self.constructing, false);
        let saved_nt = std::mem::replace(&mut self.new_target, Value::Undefined);
        self.fn_frames.push(FnFrame {
            fn_ptr: Rc::as_ptr(o) as usize,
            coro: self.cur_coro,
            strict: func.is_strict,
            extra: None,
        });
        let this_val = self.bind_compiled_this(&func, chunk, unsafe { this_slot.read() }, false);
        let mut r = unsafe {
            crate::jit::run_moved(
                self,
                chunk,
                code,
                &env as *const Env,
                this_val,
                args,
                argc,
                chunk.jit_frame(),
            )
        };
        self.fn_frames.pop();
        self.constructing = saved_ctor;
        self.new_target = saved_nt;
        // Proper-tail-call trampoline, exactly like `call`.
        while r.is_ok() {
            match self.pending_tail.take() {
                Some(bx) => {
                    let (f, t, a) = *bx;
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
        Some(r)
    }

    fn call_user_inner(
        &mut self,
        func: &Rc<Function>,
        closure: Env,
        this: Value,
        args: &[Value],
        is_construct: bool,
        fn_obj: &Gc,
    ) -> Result<Value, Abrupt> {
        // Bytecode fast call: an eligible sync callee with a compiled chunk runs on the VM with no
        // activation environment at all. Sound because a compiled body has no closures,
        // `arguments`, direct eval, `with`, `super`, or `new.target` (`bytecode::compile` refuses
        // them all): nothing can observe the activation, so free names resolve through the
        // definition env exactly as they would through an empty activation parented there.
        // Constructs qualify too when the callee is a *plain* function (no `class_info`): the
        // fresh `this` came in from `construct_nt`, which also maps a non-object return back to
        // it — and a VM body cannot rebind `this`, so the slow path's scope walk-back would find
        // the same value. Class constructors (field initializers, derived-`this` TDZ) and
        // generators/async stay on the tree-walker. For plain calls `call_dispatch` already
        // saved/cleared `new_target`/`constructing`. One divergence: no `lazy` args stash, so
        // legacy `f.arguments` reflection during an active VM frame reads null (the VM's
        // slot-based locals never aliased it faithfully anyway).
        if !matches!(self.tier, crate::bytecode::Tier::Interp)
            && !func.is_generator
            && !func.is_async
            && (!is_construct || !self.class_info.contains_key(&(Rc::as_ptr(fn_obj) as usize)))
        {
            if func.code.get().is_none() {
                let n = func.calls.get().saturating_add(1);
                func.calls.set(n);
                // A body with a loop compiles on its first call: one call can run a million
                // iterations, so the call-count threshold would leave it on the tree-walker.
                if n > self.tier_threshold || func.scan_flags() & crate::ast::SCAN_HAS_LOOP != 0 {
                    let compiled = crate::bytecode::compile(func);
                    if compiled.is_none() && std::env::var_os("LUMEN_TIER_LOG").is_some() {
                        let src = func.source.as_deref().unwrap_or("<no source>");
                        let head: String = src.chars().take(70).collect();
                        eprintln!("[tier] bail: {}", head.replace('\n', " "));
                    }
                    let _ = func.code.set(compiled);
                }
            }
            if let Some(Some(chunk)) = func.code2.get().or_else(|| func.code.get()) {
                let chunk = chunk.clone();
                let this_val = self.bind_compiled_this(func, &chunk, this, is_construct);
                // A construct consumes the pending new.target exactly like the slow path, so a
                // stale value can never be observed later; restored after the run.
                let saved_new_target = if is_construct {
                    Some(std::mem::replace(
                        &mut self.new_target,
                        std::mem::replace(&mut self.pending_new_target, Value::Undefined),
                    ))
                } else {
                    None
                };
                let r =
                    self.run_compiled_chunk(func, &chunk, &closure, this_val, args, is_construct);
                if let Some(nt) = saved_new_target {
                    self.new_target = nt;
                }
                return r;
            }
        }
        // Debug: `LUMEN_AST_HOT=1` reports functions whose bodies keep executing on the
        // tree-walker (each time the per-function call count crosses a power of ten).
        static AST_HOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *AST_HOT.get_or_init(|| std::env::var_os("LUMEN_AST_HOT").is_some()) {
            let n = func.calls.get().saturating_add(1);
            func.calls.set(n);
            if n >= 1000 && (n == 1000 || n == 10_000 || n == 100_000 || n == 1_000_000) {
                let name = func.name.as_deref().unwrap_or("<anon>");
                let src: String = func
                    .source
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                eprintln!("[ast-hot] {name} ×{n}: {}", src.replace('\n', " "));
            }
        }
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
            let scan = func.scan_flags();
            // `new.target` resolves lexically (arrows and closures created here keep seeing this
            // function's value, even after it returns). Skipped when nothing in the activation
            // can name it — Expr::NewTarget peeks up the scope chain, so an unused binding here
            // would only shadow an (equally unread) outer one.
            if scan & crate::ast::SCAN_NEW_TARGET != 0 {
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
            }
            // Class machinery and every [[Construct]] need the `this` binding regardless of the
            // body: a derived constructor's stays in TDZ until `super()` initializes it in place,
            // `super.x` resolves its receiver through it, and a constructor's non-object return
            // reads the *current* binding back out of the scope. Otherwise it exists only for
            // bodies that can name `this`.
            let is_class_fn = self.class_info.contains_key(&(Rc::as_ptr(fn_obj) as usize));
            let derived_tdz = is_construct
                && self
                    .class_info
                    .get(&(Rc::as_ptr(fn_obj) as usize))
                    .map(|ci| ci.derived)
                    .unwrap_or(false);
            if scan & crate::ast::SCAN_THIS != 0 || func.is_method || is_class_fn || is_construct {
                // Strict: pass through. Sloppy: undefined/null → global; primitive → box.
                let this_val = if func.is_strict || is_construct {
                    this
                } else {
                    match this {
                        Value::Undefined | Value::Null => Value::Obj(self.global.clone()),
                        other @ Value::Obj(_) => other,
                        // Sloppy mode: a primitive `this` is boxed to its wrapper (ToObject).
                        prim => crate::builtins::box_primitive_pub(self, prim),
                    }
                };
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
            }
            // The `arguments` exotic object is built only when the body (or a nested arrow, or a
            // possible direct eval) can name it. Otherwise a plain sloppy function stashes what a
            // reflective `fn.arguments` read would need to materialize it on demand.
            if scan & crate::ast::SCAN_ARGUMENTS != 0 {
                let ao = self.make_arguments_object(func, args, &scope, fn_obj);
                if let Some(frame) = self.fn_frames.last_mut() {
                    frame.extra.get_or_insert_with(Default::default).args_obj =
                        Value::Obj(ao.clone());
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
            } else if !func.is_strict && !func.is_generator && !func.is_async && !func.is_method {
                if let Some(frame) = self.fn_frames.last_mut() {
                    frame.extra.get_or_insert_with(Default::default).lazy =
                        Some((func.clone(), Rc::from(args), scope.clone()));
                }
            }
            // Methods and class constructors may reference `super.x` — including from a direct
            // eval in a nested arrow, which resolves this marker lexically long after the call.
            if func.is_method || self.class_info.contains_key(&(Rc::as_ptr(fn_obj) as usize)) {
                scope.borrow_mut().vars.insert(
                    "%superpropok%".to_string(),
                    Binding::data(Value::Bool(true), false, true),
                );
            }
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
            let r = self.run_async(func, &body, param_seed, args);
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
        self.hoist_fn_body(func, &body, &param_names);
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
        // (The bytecode tier intercepted eligible calls at the top of this function; anything
        // reaching here — construct calls, uncompilable bodies — runs on the tree-walker.)
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
            // A coroutine body runs outside `Interp::call`'s tail-call trampoline, so its top-level
            // `return f(...)` are never proper tail calls; force `tco_ok` off before each statement
            // (it can leak back to `true` across a `yield`/`await` resume) so a return here can't be
            // parked as a pending tail call that nothing runs. See the note in `run_async`.
            let saved_tco = std::mem::replace(&mut i.tco_ok, false);
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
                i.tco_ok = false;
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
            i.in_async_gen_body = saved_agb;
            i.strict = saved_strict;
            i.tco_ok = saved_tco;
            outcome
        });
        let ptr = self as *mut Interp;
        let coro = match crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(body)) {
            Ok(c) => c,
            Err(_) => {
                return Err(Abrupt::Throw(
                    self.make_error("Error", crate::coroutine::UNSUPPORTED_MSG),
                ))
            }
        };
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
    /// The compiled chunk for an async function if it should run on the bytecode VM: the bytecode
    /// tier is on and the body has been called past the tier threshold and fits the VM subset.
    /// Mirrors the sync-call tiering in `call_inner`.
    fn async_vm_chunk(&self, func: &Rc<Function>) -> Option<Rc<crate::bytecode::Chunk>> {
        if matches!(self.tier, crate::bytecode::Tier::Interp) {
            return None;
        }
        if func.code.get().is_none() {
            let n = func.calls.get().saturating_add(1);
            func.calls.set(n);
            // Loop-bearing bodies compile on first call, like the sync path.
            if n > self.tier_threshold || func.scan_flags() & crate::ast::SCAN_HAS_LOOP != 0 {
                let _ = func.code.set(crate::bytecode::compile(func));
            }
        }
        match func.code2.get().or_else(|| func.code.get()) {
            Some(Some(chunk)) => Some(chunk.clone()),
            _ => None,
        }
    }

    fn run_async(
        &mut self,
        func: &Rc<Function>,
        scope: &Env,
        param_seed: Option<Env>,
        args: &[Value],
    ) -> Result<Value, Abrupt> {
        // Fast path: an async body that compiles runs on the bytecode VM, suspending at each `await`
        // without an OS-thread coroutine (see `bytecode::VmCoro`). Params seed straight into slots
        // from `args`; the activation scope is only the root for free-name resolution.
        let coro = if let Some(chunk) = self.async_vm_chunk(func) {
            let this_val = if chunk.uses_this() {
                self.get_var("this", scope)?
            } else {
                Value::Undefined
            };
            crate::coroutine::Coroutine::Vm(crate::bytecode::VmCoro::new(
                self,
                chunk,
                scope.clone(),
                this_val,
                args,
            ))
        } else {
            self.spawn_async_thread(func, scope, param_seed)?
        };
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

    /// The tree-walker fallback for an async body that did not compile: run it on a pooled OS-thread
    /// coroutine.
    fn spawn_async_thread(
        &mut self,
        func: &Rc<Function>,
        scope: &Env,
        param_seed: Option<Env>,
    ) -> Result<crate::coroutine::Coroutine, Abrupt> {
        let func = func.clone();
        let scope = scope.clone();
        let body: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> = Box::new(move |i| {
            let saved_strict = i.strict;
            i.strict = func.is_strict;
            // A coroutine body runs outside `Interp::call`'s tail-call trampoline, so none of its
            // top-level `return f(...)` are proper tail calls: parking one as a pending tail call
            // would leave nothing to run it and resolve the body to `undefined`. `tco_ok` can leak
            // back to `true` across an `await`/resume (it is ambient interpreter state), so force it
            // off before *each* statement rather than just once — a `return` reads `tco_ok` at its
            // very start, so this makes the body's own returns take the ordinary path.
            let saved_tco = std::mem::replace(&mut i.tco_ok, false);
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
                i.tco_ok = false;
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
            i.tco_ok = saved_tco;
            outcome
        });
        let ptr = self as *mut Interp;
        match crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(body)) {
            Ok(c) => Ok(c),
            Err(_) => Err(Abrupt::Throw(
                self.make_error("Error", crate::coroutine::UNSUPPORTED_MSG),
            )),
        }
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
        if coro.done() {
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
        if !coro.started() {
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
    pub(crate) fn construct_nt(
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
            Callable::Native(_) | Callable::NativeData(_) => {
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
                let r = self
                    .dispatch_native(&call, Value::Undefined, args)
                    .map_err(Abrupt::Throw);
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
                    matches!(p.getter(), None | Some(Value::Undefined))
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
                    matches!(p.setter(), None | Some(Value::Undefined))
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

    pub(crate) fn run_program(&mut self, body: &[Stmt]) -> Result<Value, Value> {
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
        let ops = collect_hoist_ops(stmts, self.strict, param_blocked);
        self.apply_hoist_ops(&ops, scope);
    }

    /// [`Self::hoist`] for a function body, with the hoisting plan cached on the [`Function`]
    /// (the body and parameter names never change, so the plan is computed once per function).
    pub(crate) fn hoist_fn_body(
        &mut self,
        func: &Rc<Function>,
        scope: &Env,
        param_blocked: &[String],
    ) {
        let strict = self.strict;
        let cached = func.hoist.get_or_init(|| {
            (
                strict,
                Rc::new(collect_hoist_ops(&func.body, strict, param_blocked)),
            )
        });
        if cached.0 == strict {
            let ops = cached.1.clone();
            self.apply_hoist_ops(&ops, scope);
        } else {
            // Strictness differs from the cached computation (shouldn't happen — a function's
            // strictness is fixed at parse) — fall back to an uncached walk.
            let ops = collect_hoist_ops(&func.body, strict, param_blocked);
            self.apply_hoist_ops(&ops, scope);
        }
    }

    /// Replay a hoisting plan against `scope` (see [`HoistOp`] for the op semantics).
    fn apply_hoist_ops(&mut self, ops: &[HoistOp], scope: &Env) {
        for op in ops {
            match op {
                HoistOp::Var(name) => {
                    if !scope.borrow().vars.contains_key(name.as_str()) {
                        scope
                            .borrow_mut()
                            .vars
                            .insert(name.clone(), undef_var_binding());
                    }
                }
                HoistOp::VarForce(name) => {
                    scope
                        .borrow_mut()
                        .vars
                        .insert(name.clone(), undef_var_binding());
                }
                HoistOp::Fn(name, func) => {
                    let f = self.make_function(func.clone(), scope.clone());
                    if name == "*default*" {
                        self.set_fn_name(&f, "default");
                    }
                    scope.borrow_mut().vars.insert(
                        name.clone(),
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
                HoistOp::AnnexB(name, func) => {
                    if !scope.borrow().vars.contains_key(name.as_str()) {
                        scope
                            .borrow_mut()
                            .vars
                            .insert(name.clone(), undef_var_binding());
                    }
                    self.annexb_fn_sync
                        .insert(Rc::as_ptr(func) as usize, func.clone());
                }
            }
        }
    }
}

/// The slice of a [`Property`] a prototype-chain read needs — cloned out instead of the whole
/// Property so hot data reads pay for one Rc bump, not three.
struct PropRead {
    accessor: bool,
    get: Option<Value>,
    value: Value,
}

/// The undefined, mutable, function-scoped binding hoisting creates for a `var` name.
fn undef_var_binding() -> Binding {
    Binding {
        value: Value::Undefined,
        mutable: true,
        strict_immutable: false,
        initialized: true,
        import_ref: None,
        deletable: false,
    }
}

/// Build the hoisting plan for a statement list: `var` names in traversal order, then function
/// declarations in source order, then (sloppy mode) Annex B block-function promotions. Pure — the
/// plan for a function body is cached on its [`Function`] and replayed each call.
pub(crate) fn collect_hoist_ops(
    stmts: &[Stmt],
    strict: bool,
    param_blocked: &[String],
) -> Vec<HoistOp> {
    let mut out = Vec::new();
    for stmt in stmts {
        collect_hoist_stmt(stmt, &mut out);
    }
    // Function declarations are also initialised eagerly (in source order, after var names). An
    // anonymous `export default function(){}` is a HoistableDeclaration too, bound to
    // `*default*`. Annex B: labels over a top-level function declaration are transparent.
    for stmt in stmts {
        let mut inner = unwrap_export(stmt);
        while let (false, Stmt::Labeled { body, .. }) = (strict, inner) {
            inner = body;
        }
        if let Stmt::FuncDecl(func) = inner {
            let name = match &func.name {
                Some(n) => n.clone(),
                None if matches!(stmt, Stmt::ExportDefault(_)) => "*default*".to_string(),
                None => continue,
            };
            out.push(HoistOp::Fn(name, func.clone()));
        }
    }
    // Annex B.3.3: in sloppy mode, a function declared inside a block (or an if/label
    // substatement position) also gets a `var`-style binding in the enclosing function/global
    // scope — initialized to undefined, synced with the block binding when the declaration is
    // evaluated — unless a lexical declaration between here and the block makes the equivalent
    // `var` an early error.
    if !strict {
        let mut blocked: Vec<String> = block_lexical_names(stmts);
        blocked.extend(param_blocked.iter().cloned());
        for stmt in stmts {
            collect_annexb_funcs(stmt, false, &mut blocked, &mut out);
        }
    }
    out
}

/// Hoist `var` declarations from one statement (function declarations and Annex B promotions are
/// collected by the later phases). `for`/`for-in/of` heads bind unconditionally (VarForce),
/// matching the runtime behavior this replaced.
fn collect_hoist_stmt(stmt: &Stmt, out: &mut Vec<HoistOp>) {
    match stmt {
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => collect_hoist_stmt(inner, out),
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (pat, _) in decls {
                let mut names = Vec::new();
                pattern_idents(pat, &mut names);
                out.extend(names.into_iter().map(HoistOp::Var));
            }
        }
        Stmt::If { cons, alt, .. } => {
            collect_hoist_stmt(cons, out);
            if let Some(a) = alt {
                collect_hoist_stmt(a, out);
            }
        }
        Stmt::Block(body) => {
            for s in body {
                collect_hoist_stmt(s, out);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::With { body, .. } => collect_hoist_stmt(body, out),
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
                        out.extend(names.into_iter().map(HoistOp::VarForce));
                    }
                }
            }
            collect_hoist_stmt(body, out);
        }
        Stmt::ForInOf {
            decl: Some(DeclKind::Var),
            left,
            body,
            ..
        } => {
            let mut names = Vec::new();
            pattern_idents(left, &mut names);
            out.extend(names.into_iter().map(HoistOp::VarForce));
            collect_hoist_stmt(body, out);
        }
        Stmt::ForInOf { body, .. } => collect_hoist_stmt(body, out),
        // A switch is a single block scope, but `var`s in its case bodies still hoist to the
        // enclosing function/script (only lexical bindings are switch-block-scoped).
        Stmt::Switch { cases, .. } => {
            for case in cases {
                for s in &case.body {
                    collect_hoist_stmt(s, out);
                }
            }
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            for s in block {
                collect_hoist_stmt(s, out);
            }
            if let Some((_, h)) = handler {
                for s in h {
                    collect_hoist_stmt(s, out);
                }
            }
            if let Some(f) = finalizer {
                for s in f {
                    collect_hoist_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

/// Annex B.3.3 collection (sloppy mode): a *plain* function declaration in a block/if/label
/// substatement position promotes to a var binding unless a lexical name in between blocks it.
fn collect_annexb_funcs(
    stmt: &Stmt,
    in_block: bool,
    blocked: &mut Vec<String>,
    out: &mut Vec<HoistOp>,
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
                out.push(HoistOp::AnnexB(name.clone(), func.clone()));
            }
        }
        Stmt::Block(body) => {
            let added = block_lexical_names(body);
            let pushed = push_blocked(blocked, added);
            // The block's DIRECT function declarations hoist first (their own/sibling names
            // don't block them)...
            for s in body {
                if matches!(s, Stmt::FuncDecl(_)) {
                    collect_annexb_funcs(s, true, blocked, out);
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
                    collect_annexb_funcs(s, true, blocked, out);
                }
            }
            blocked.truncate(blocked.len() - pushed_fns);
            blocked.truncate(blocked.len() - pushed);
        }
        Stmt::If { cons, alt, .. } => {
            collect_annexb_funcs(cons, true, blocked, out);
            if let Some(a) = alt {
                collect_annexb_funcs(a, true, blocked, out);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            collect_annexb_funcs(body, true, blocked, out)
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
            collect_annexb_funcs(body, true, blocked, out);
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
            collect_annexb_funcs(body, true, blocked, out);
            blocked.truncate(blocked.len() - pushed);
        }
        Stmt::Labeled { body, .. } | Stmt::With { body, .. } => {
            collect_annexb_funcs(body, true, blocked, out)
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
                    collect_annexb_funcs(s, true, blocked, out);
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
                    collect_annexb_funcs(s, true, blocked, out);
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
                    collect_annexb_funcs(s, true, blocked, out);
                }
                blocked.truncate(blocked.len() - pushed);
            }
            if let Some(f) = finalizer {
                let added = block_lexical_names(f);
                let pushed = push_blocked(blocked, added);
                for s in f {
                    collect_annexb_funcs(s, true, blocked, out);
                }
                blocked.truncate(blocked.len() - pushed);
            }
        }
        _ => {}
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
