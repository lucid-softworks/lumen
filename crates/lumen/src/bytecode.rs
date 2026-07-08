//! Bytecode tier v0: a per-function stack VM behind the tree-walking interpreter.
//!
//! The tree-walker is the reference oracle — it passes 100% of test262 and its semantics are
//! never altered by this tier. A function is either compiled *whole* (its body contains only
//! constructs this compiler fully understands) or it runs in the tree-walker; there is no partial
//! compilation and no deoptimization. Every operation with observable semantics (property access,
//! calls, coercions, name resolution outside the function) delegates to the interpreter's own
//! helpers, so behavior differences can only come from the local-variable and dispatch layers.
//!
//! Locals live in a flat slot vector: v0 refuses any function where a local could be observed
//! from outside (inner functions/closures, direct `eval`, `with`, `arguments`, destructuring),
//! which is what makes slot storage sound. TDZ is represented by `Value::Empty` in the slot —
//! reads check it and throw the same ReferenceError the tree-walker would.
//!
//! Tier selection (see `Interp::tier`): `interp` (default — this module is never entered),
//! `bytecode` (compile at `tier_threshold` calls; 0 = immediately). Selectable via the `LUMEN_TIER`
//! / `LUMEN_TIER_THRESHOLD` env vars, the CLI's `--tier`, or `Engine::set_tier`.

use std::rc::Rc;

use crate::ast::*;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

/// Execution tier. `Interp` must not touch any codegen path at all; `Jit` compiles eligible
/// chunks to ARM64 machine code (macOS/Apple Silicon), falling back to the bytecode VM.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Interp,
    Bytecode,
    Jit,
}

/// Per-site property inline-cache state. `depth == IC_EMPTY` means the site has not cached yet.
/// Otherwise the property was last found as an own, non-accessor data property of the object
/// `depth` prototype hops above the receiver, at `entries` slot `slot` — and every hop below the
/// holder had *no* own property of that name. A hit re-validates all of that (each hop plain and
/// missing `name`, the holder's cached slot still keyed `name`), so a stale cache — including a
/// *different* object reaching this shared per-site cache — can only cost time, never correctness.
///
/// `recv_shape` / `holder_shape` are the receiver's and holder's [object shapes] at cache time.
/// For a `depth == 0` or `depth == 1` hit on non-exotic objects they turn validation into shape-
/// id compares (no per-hop key/hash checks): shapes are shared across structurally-identical
/// objects, so matching one recorded from object A on object B guarantees B's `slot` maps `name`
/// too. Deeper hits, exotics (arrays), and shape misses fall back to the key-checked walk.
///
/// [object shapes]: crate::value::Props::shape
///
/// `repr(C)` with this field order gives the JIT's inline templates fixed byte offsets to read
/// the live cache from machine code: recv_shape@0, holder_shape@4, slot@8, depth@12.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IcState {
    pub recv_shape: u32,
    pub holder_shape: u32,
    pub slot: u32,
    pub depth: u8,
}

/// Byte offsets into an [`IcState`] `Cell`, for the JIT inline templates.
pub const IC_OFF_RECV_SHAPE: u32 = 0;
pub const IC_OFF_HOLDER_SHAPE: u32 = 4;
pub const IC_OFF_SLOT: u32 = 8;
pub const IC_OFF_DEPTH: u32 = 12;

pub const IC_EMPTY: u8 = u8::MAX;
/// Deepest prototype hop the IC will record; hotter sites deeper than this stay on the slow path.
pub const IC_MAX_DEPTH: u8 = 4;

/// Per-site free-name inline-cache state (`LoadName` / `LoadNameForCall`): the last successful
/// *depth-0* resolution — the name was found directly in the scope the chunk runs under (`env`),
/// as a plain initialized binding (no `with` object on that scope, no live module import).
///
/// A hit revalidates: (1) the current env is *the same allocation* — `env` compares raw pointers,
/// which is ABA-safe because `Chunk::name_pins` holds a `Weak` to the cached scope, pinning its
/// allocation for the cache's lifetime; (2) the scope's [`crate::interpreter::VarMap`] generation
/// is unchanged — every structural map mutation bumps it, so `binding` still points at the live
/// entry *and* no insert/remove could have changed what the name resolves to. Depth-0-only is
/// what makes the generation check complete: with no intermediate scopes between start and
/// holder, there is nothing else whose mutation could re-route the name (a sloppy direct `eval`
/// hoisting into this scope, or a `delete`, is an insert/remove here and bumps the generation).
///
/// In-place binding writes don't bump the generation, so a hit reads the *live* value and the
/// live `initialized` flag through the pointer — both exactly what the slow path would see.
///
/// `repr(C)` with this field order gives the JIT template fixed offsets: env@0, binding@8, gen@16.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct NameIc {
    /// `Rc::as_ptr` of the scope at cache time (0 = empty).
    pub env: usize,
    /// The resolved `&Binding` within that scope's map.
    pub binding: usize,
    pub gen: u32,
    pub _pad: u32,
}

impl NameIc {
    pub const EMPTY: NameIc = NameIc {
        env: 0,
        binding: 0,
        gen: 0,
        _pad: 0,
    };
}

/// Byte offsets into a [`NameIc`] `Cell`, for the JIT inline template.
pub const NAME_IC_OFF_ENV: u32 = 0;
pub const NAME_IC_OFF_BINDING: u32 = 8;
pub const NAME_IC_OFF_GEN: u32 = 16;

impl IcState {
    pub const EMPTY: IcState = IcState {
        recv_shape: 0,
        holder_shape: 0,
        slot: 0,
        depth: IC_EMPTY,
    };
}

/// Which update `UpdateLocal` performs, and the value it leaves on the stack: `Pre*` push the
/// updated value, `Post*` push the original (coerced) value, `*Discard` push nothing (the update
/// is a statement or a `for` update — its value is unobservable).
#[derive(Clone, Copy, Debug)]
pub enum UpdKind {
    PreInc,
    PreDec,
    PostInc,
    PostDec,
    IncDiscard,
    DecDiscard,
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Const(u32),
    Undef,
    Dup,
    Pop,
    LoadLocal(u16),
    StoreLocal(u16),
    /// Read a captured local from the activation environment (TDZ-checked). The operand indexes
    /// `names`; the activation env holds exactly the captured bindings, so this is one hash hit.
    LoadCap(u32),
    /// Write a captured local (TDZ-checked: assignment before a lexical's initialization throws).
    StoreCap(u32),
    /// Initialize a captured lexical (`let`/`const` declaration): sets the value and clears TDZ.
    StoreCapInit(u32),
    /// `++`/`--` on a captured local, in place (the env-homed `UpdateLocal`).
    UpdateCap(u32, UpdKind),
    /// Create a closure over the current environment from `Chunk::funcs[fidx]`. The second
    /// operand names an anonymous function expression per NamedEvaluation (`names` index, or
    /// `u32::MAX` for none).
    MakeClosure(u32, u32),
    /// `++`/`--` on a local slot, done in place (no LoadLocal/Plus/Add dance). Applies ToNumeric
    /// so a BigInt slot stays a BigInt — the `Plus`-based lowering this replaces was ToNumber and
    /// wrongly threw on BigInt. The `UpdKind` says increment vs decrement and which value (old,
    /// new, or none in statement position) to leave on the stack.
    UpdateLocal(u16, UpdKind),
    /// Put the slot into its temporal dead zone (block entry for `let`/`const`).
    Tdz(u16),
    /// Read a free name (resolved through the scope chain / global). Operands: name index,
    /// per-site [`NameIc`] index into `Chunk::name_caches`.
    LoadName(u32, u32),
    StoreName(u32),
    LoadThis,
    /// `obj.name`. First operand is the name index; second is the per-site inline-cache index into
    /// `Chunk::caches` (see `Interp::get_prop_ic`).
    GetProp(u32, u32),
    /// `obj.name = v`. Operands: name index, inline-cache index.
    SetProp(u32, u32),
    /// `obj.name = v` in statement position: stores without leaving `v` on the stack.
    SetPropDrop(u32, u32),
    GetElem,
    SetElem,
    /// `obj[k] = v` in statement position: stores without leaving `v` on the stack.
    SetElemDrop,
    /// `x[k]` where `x` is a *parameter* slot: fused LoadLocal+GetElem — the receiver never
    /// crosses the operand stack (no clone/drop refcount churn). Reads the slot at exec time,
    /// which is only sound because the emitter proves the key expression cannot reassign the
    /// base local (see `Compiler::fused_elem_slot`) and a parameter is never in TDZ.
    GetElemLocal(u16),
    /// `x[k] = v` with `x` a parameter slot (see [`Op::GetElemLocal`]), keeping `v` on the stack.
    SetElemLocal(u16),
    /// `x[k] = v` in statement position with `x` a parameter slot.
    SetElemLocalDrop(u16),
    /// `obj.name++` / `--obj.name` as one op: pops obj, reads via the site IC, ToNumeric, ±1,
    /// writes back via the IC, pushes old / new / nothing per `UpdKind`.
    UpdateProp(u32, u32, UpdKind),
    /// `obj[k]++` / `--obj[k]`: pops k and obj, coerces the key at most once (matching the
    /// oracle's cached-Reference semantics), read-modify-write, pushes per `UpdKind`.
    UpdateElem(UpdKind),
    /// Compound `obj[k] op= v` support: coerce the top of stack to a property key *now* when the
    /// coercion could be observable (an object's valueOf/toString), so the following GetElem +
    /// SetElem pair can't run it twice. Num/Str keys stay raw — their later coercion is
    /// side-effect-free and deterministic, and keeping numbers numeric preserves the dense-array
    /// fast path. Checks the base (one below top) for null/undefined first, like `ref_prop_key`.
    ToPropKey,
    /// Duplicate the top two stack values (for compound `obj[k] op= v`).
    Dup2,
    /// `obj.name` as a call target: pops obj, pushes obj then the method (get runs before args).
    /// Operands: name index, inline-cache index (methods live on prototypes — the IC walks hops).
    GetMethod(u32, u32),
    /// `obj[k]` as a call target: pops k and obj, pushes obj then the method.
    GetMethodElem,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    NotEq,
    StrictEq,
    StrictNotEq,
    /// Any other binary operator (`**`, `in`, `instanceof`) via the interpreter, op in names.
    GenBin(u32),
    Neg,
    Plus,
    Not,
    BitNot,
    Typeof,
    Void,
    Jump(u32),
    JumpIfFalse(u32),
    /// Peek variants leave the operand on the stack (for `&&` / `||` / `??`).
    JumpIfFalsePeek(u32),
    JumpIfTruePeek(u32),
    JumpIfNotNullishPeek(u32),
    /// Plain call: pops argc args and the callee; `this` is undefined.
    Call(u16),
    /// Resolve a free name as a call target *before* the arguments evaluate (spec order):
    /// pushes the `with`-object `this` (or undefined) then the callee, feeding CallWithThis.
    /// Operands: name index, per-site [`NameIc`] index.
    LoadNameForCall(u32, u32),
    /// Method call: pops argc args, the method, and the receiver pushed by GetMethod*.
    CallWithThis(u16),
    New(u16),
    MakeArray(u16),
    /// Object literal: `count` plain data keys starting at names[start], values on the stack.
    MakeObject(u32, u16),
    Throw,
    Return,
    ReturnUndef,
    /// `await expr`: suspend the async body, handing the popped operand to the driver; on resume the
    /// settled value is pushed back (or a rejection is thrown). Only emitted for async functions.
    Await,
    /// Enter a `try` region: register a handler that, on a throw anywhere in the region, unwinds the
    /// stack and jumps to the operand (the catch pc, with the exception pushed).
    PushHandler(u32),
    /// Leave a `try` region without throwing: drop the innermost handler.
    PopHandler,
}

/// An active `try` region on the VM's handler stack.
struct Handler {
    /// Where to jump on a throw (the catch entry).
    catch_pc: usize,
    /// The operand-stack depth to unwind to before pushing the exception.
    stack_depth: usize,
}

/// How one captured binding seeds into the activation environment at entry (in order).
pub(crate) enum CapInit {
    /// A captured parameter: seed from argument `k`.
    Param(u16, Rc<str>),
    /// A captured function-scoped `var`: undefined, unless already bound (a same-named param).
    Var(Rc<str>),
    /// A hoisted function declaration: a closure over the activation itself (self-recursion).
    Fn(u16, Rc<str>),
    /// A captured top-level lexical: inserted uninitialized (TDZ); bool = `const`.
    Lexical(Rc<str>, bool),
}

pub struct Chunk {
    // (fields below; Debug is manual — `consts` holds engine Values)
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    n_slots: usize,
    /// Slot names, for TDZ ReferenceError messages.
    slot_names: Vec<Rc<str>>,
    /// Parameter positions map onto slots [0, n_params).
    n_params: usize,
    /// Slots reset to undefined after parameter seeding (the tree-walker's `for`-head var
    /// hoisting overwrites same-named params; replicated bug-for-bug — it is the oracle).
    var_force_resets: Vec<u16>,
    uses_this: bool,
    /// Inner function templates for `MakeClosure`.
    funcs: Vec<Rc<Function>>,
    /// Captured bindings to seed into a fresh activation env at entry; empty = no activation
    /// needed (closures, if any, capture the definition env directly).
    cap_inits: Vec<CapInit>,
    /// An inner arrow chain reads the outer `this`: the activation carries a `this` binding.
    env_this: bool,
    /// One inline-cache slot per property-access op (`GetProp`/`SetProp`/`SetPropDrop`/`GetMethod`),
    /// holding the (prototype depth, `entries` slot) last seen for that site (see [`IcState`]). The
    /// `Chunk` is shared across calls via `Rc`, so these persist. `Cell` is fine: the VM runs one
    /// thread at a time (coroutine ping-pong), like the rest of the engine's shared-`Rc` state.
    caches: Vec<std::cell::Cell<IcState>>,
    /// One [`NameIc`] slot per free-name op (`LoadName`/`LoadNameForCall`), persisting across
    /// calls like `caches`.
    name_caches: Vec<std::cell::Cell<NameIc>>,
    /// Weak handles pinning each name cache's scope allocation (parallel to `name_caches`), so
    /// the cached raw `env` pointer can never be recycled into a different scope while cached.
    name_pins: std::cell::RefCell<Vec<Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>>>,
    /// Machine-code tier state: the compile result once attempted (`None` inside = the chunk
    /// cannot JIT — async, or an unsupported platform — and runs on the bytecode VM forever).
    pub(crate) jit: std::cell::OnceCell<Option<Rc<crate::jit::JitCode>>>,
}

impl Chunk {
    /// Whether the body (or an inner arrow chain) reads `this`, so the caller must bind it.
    pub fn uses_this(&self) -> bool {
        self.uses_this || self.env_this
    }
    /// Whether calls need a real activation environment (captured locals / lexical `this`).
    fn needs_env(&self) -> bool {
        !self.cap_inits.is_empty() || self.env_this
    }

    /// Build the activation environment for one call: a fresh scope under `env` holding exactly
    /// the captured bindings (and `this` when an inner arrow chain reads it). Free names and
    /// `MakeClosure` environments route through it. Returns `env` untouched when nothing is
    /// captured — closures then capture the definition env directly, which resolves identically
    /// because none of their free names are outer locals.
    fn make_run_env(&self, i: &Interp, env: &Env, this_val: &Value, args: &[Value]) -> Env {
        if !self.needs_env() {
            return env.clone();
        }
        let act = crate::interpreter::new_var_scope(Some(env.clone()));
        // Function-declaration closures capture the activation itself, so they are created after
        // the borrow below is released.
        let mut fns: Vec<(u16, Rc<str>)> = Vec::new();
        {
            let mut b = act.borrow_mut();
            for ci in &self.cap_inits {
                match ci {
                    CapInit::Param(k, name) => {
                        b.vars.insert(
                            name.to_string(),
                            crate::interpreter::Binding {
                                value: args.get(*k as usize).cloned().unwrap_or(Value::Undefined),
                                mutable: true,
                                strict_immutable: false,
                                initialized: true,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                    CapInit::Var(name) => {
                        if !b.vars.contains_key(&**name) {
                            b.vars.insert(
                                name.to_string(),
                                crate::interpreter::Binding {
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
                    CapInit::Fn(fidx, name) => fns.push((*fidx, name.clone())),
                    CapInit::Lexical(name, is_const) => {
                        b.vars.insert(
                            name.to_string(),
                            crate::interpreter::Binding {
                                value: Value::Undefined,
                                mutable: !is_const,
                                strict_immutable: *is_const,
                                initialized: false,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
            }
            if self.env_this {
                b.vars.insert(
                    "this".to_string(),
                    crate::interpreter::Binding {
                        value: this_val.clone(),
                        mutable: false,
                        strict_immutable: true,
                        initialized: true,
                        import_ref: None,
                        deletable: false,
                    },
                );
            }
        }
        for (fidx, name) in fns {
            let v = i.make_function(self.funcs[fidx as usize].clone(), act.clone());
            act.borrow_mut().vars.insert(
                name.to_string(),
                crate::interpreter::Binding {
                    value: v,
                    mutable: true,
                    strict_immutable: false,
                    initialized: true,
                    import_ref: None,
                    deletable: false,
                },
            );
        }
        act
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({} ops, {} slots)", self.ops.len(), self.n_slots)
    }
}

// ---------------------------------------------------------------------------------------------
// Capture analysis
// ---------------------------------------------------------------------------------------------

/// Which names the body's *inner functions* can resolve to the outer function's locals — the set
/// that must live in a real activation environment instead of VM slots. Also whether any inner
/// arrow chain reads the outer `this`.
///
/// Soundness rule: a name wrongly treated as local to an inner function would silently resolve
/// past the activation to the wrong binding, so everything not fully understood returns `None`
/// (direct eval, `with`, sloppy block function declarations, module syntax, …) and the caller
/// bails to the tree-walker.
struct CaptureScan {
    /// Declared-name scopes, innermost last, each tagged with the function-nesting depth it
    /// belongs to (0 = the function being compiled).
    scopes: Vec<(std::collections::HashSet<String>, u32)>,
    fn_depth: u32,
    /// Names resolving from depth > 0 to a depth-0 scope.
    captured: std::collections::HashSet<String>,
    /// Names declared by a depth-0 scope that is NOT the function's top scope (block lexicals,
    /// for-head lexicals, catch params). If one of these is captured, the per-block binding
    /// freshness slots can't express — the caller bails.
    depth0_inner_decls: std::collections::HashSet<String>,
    /// Whether `this` is read from an inner arrow chain rooted at the outer function.
    env_this: bool,
    /// Arrow-ness of each enclosing function on the current path (index 0 = the outer function).
    arrow_path: Vec<bool>,
}

/// Collect every binding a `Pattern` introduces.
fn pat_idents(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Ident(n) => {
            out.insert(n.clone());
        }
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, .. } => pat_idents(pattern, out),
                    ArrayPatElem::Rest(p) => pat_idents(p, out),
                }
            }
        }
        Pattern::Object(o) => {
            for pr in &o.props {
                pat_idents(&pr.value, out);
            }
            if let Some(r) = &o.rest {
                out.insert(r.clone());
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Collect the function-scoped `var` names (and direct top-level function-declaration names) of a
/// body: recurses through blocks/loops/switch/try but never into nested functions or classes.
/// `top` distinguishes direct body statements (whose FuncDecls hoist) from block-level ones.
/// Returns false on a construct whose hoisting we don't model (sloppy Annex B block functions).
fn hoisted_vars(stmts: &[Stmt], top: bool, strict: bool, out: &mut std::collections::HashSet<String>) -> bool {
    for s in stmts {
        if !hoisted_vars_stmt(s, top, strict, out) {
            return false;
        }
    }
    true
}

fn hoisted_vars_stmt(s: &Stmt, top: bool, strict: bool, out: &mut std::collections::HashSet<String>) -> bool {
    match s {
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (p, _) in decls {
                pat_idents(p, out);
            }
            true
        }
        Stmt::FuncDecl(f) => {
            if top {
                if let Some(n) = &f.name {
                    out.insert(n.clone());
                }
                true
            } else {
                // Block-level function declaration: strict = block-scoped lexical (handled by the
                // block scope in the walker); sloppy = Annex B promotion we don't model — bail.
                strict
            }
        }
        Stmt::Block(b) => hoisted_vars(b, false, strict, out),
        Stmt::If { cons, alt, .. } => {
            hoisted_vars_stmt(cons, false, strict, out)
                && alt
                    .as_deref()
                    .map(|a| hoisted_vars_stmt(a, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::VarDecl {
                kind: DeclKind::Var,
                decls,
            }) = init.as_deref()
            {
                for (p, _) in decls {
                    pat_idents(p, out);
                }
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::ForInOf {
            decl, left, body, ..
        } => {
            if matches!(decl, Some(DeclKind::Var)) {
                pat_idents(left, out);
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            hoisted_vars(block, false, strict, out)
                && handler
                    .as_ref()
                    .map(|(_, b)| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
                && finalizer
                    .as_ref()
                    .map(|b| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::Switch { cases, .. } => cases
            .iter()
            .all(|c| hoisted_vars(&c.body, false, strict, out)),
        _ => true,
    }
}

impl CaptureScan {
    /// Analyze `func`, returning (captured names, inner-arrow-reads-this) or `None` to bail.
    fn run(func: &Function) -> Option<(std::collections::HashSet<String>, bool)> {
        let mut sc = CaptureScan {
            scopes: Vec::new(),
            fn_depth: 0,
            captured: Default::default(),
            depth0_inner_decls: Default::default(),
            env_this: false,
            arrow_path: vec![func.is_arrow],
        };
        sc.fn_body(func)?;
        // A captured name declared by an inner depth-0 scope needs per-block freshness.
        if sc.captured.iter().any(|n| sc.depth0_inner_decls.contains(n)) {
            return None;
        }
        Some((sc.captured, sc.env_this))
    }

    fn push_scope(&mut self, names: std::collections::HashSet<String>) {
        if self.fn_depth == 0 && !self.scopes.is_empty() {
            for n in &names {
                self.depth0_inner_decls.insert(n.clone());
            }
        }
        self.scopes.push((names, self.fn_depth));
    }

    /// Walk a whole function: params + hoisted vars + top-level lexicals in one scope, then body.
    fn fn_body(&mut self, func: &Function) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        for p in &func.params {
            pat_idents(&p.pattern, &mut names);
        }
        if !func.is_arrow {
            names.insert("arguments".to_string());
        }
        if func.is_fn_expr {
            if let Some(n) = &func.name {
                names.insert(n.clone());
            }
        }
        if !hoisted_vars(&func.body, true, func.is_strict, &mut names) {
            return None;
        }
        self.declare_lexicals(&func.body, &mut names);
        self.push_scope(names);
        // Parameter defaults evaluate in the function scope.
        for p in &func.params {
            if let Some(d) = &p.default {
                self.expr(d)?;
            }
        }
        for s in &func.body {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    /// Add a statement list's block-scoped declarations (let/const/class, strict block functions).
    fn declare_lexicals(&self, stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    decls,
                } => {
                    for (p, _) in decls {
                        pat_idents(p, out);
                    }
                }
                Stmt::ClassDecl(c) => {
                    if let Some(n) = &c.name {
                        out.insert(n.clone());
                    }
                }
                Stmt::FuncDecl(f) => {
                    // Only reached for *block-level* declarations (top-level ones are in the
                    // hoisted set); strict mode makes them block lexicals. (Sloppy already bailed
                    // in hoisted_vars.)
                    if let Some(n) = &f.name {
                        out.insert(n.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt]) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        self.declare_lexicals(stmts, &mut names);
        self.push_scope(names);
        for s in stmts {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    fn reference(&mut self, name: &str) {
        for (scope, depth) in self.scopes.iter().rev() {
            if scope.contains(name) {
                if *depth == 0 && self.fn_depth > 0 {
                    self.captured.insert(name.to_string());
                }
                return;
            }
        }
        // Unresolved: a global/free name of the whole compilation — nothing to capture.
    }

    /// Walk a pattern in *assignment* position (destructuring assignment): idents are references.
    fn pat_targets(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_targets(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_targets(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_targets(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                if let Some(r) = &o.rest {
                    self.reference(r);
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    /// Walk the expressions inside a *declaration* pattern (defaults, computed keys); the idents
    /// themselves were declared by the enclosing scope construction.
    fn pat_decl_exprs(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(_) => Some(()),
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_decl_exprs(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_decl_exprs(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_decl_exprs(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::VarDecl { decls, .. } => {
                for (p, init) in decls {
                    self.pat_decl_exprs(p)?;
                    if let Some(e) = init {
                        self.expr(e)?;
                    }
                }
                Some(())
            }
            Stmt::FuncDecl(f) => self.inner_fn(f),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e)?;
                }
                Some(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                self.stmt(cons)?;
                if let Some(a) = alt {
                    self.stmt(a)?;
                }
                Some(())
            }
            Stmt::Block(b) => self.block(b),
            Stmt::While { test, body } => {
                self.expr(test)?;
                self.stmt(body)
            }
            Stmt::DoWhile { body, test } => {
                self.stmt(body)?;
                self.expr(test)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let mut names = std::collections::HashSet::new();
                if let Some(ForInit::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                }) = init.as_deref()
                {
                    for (p, _) in decls {
                        pat_idents(p, &mut names);
                    }
                }
                self.push_scope(names);
                let r = (|| {
                    match init.as_deref() {
                        Some(ForInit::VarDecl { decls, .. }) => {
                            for (p, e) in decls {
                                self.pat_decl_exprs(p)?;
                                if let Some(e) = e {
                                    self.expr(e)?;
                                }
                            }
                        }
                        Some(ForInit::Expr(e)) => self.expr(e)?,
                        None => {}
                    }
                    if let Some(t) = test {
                        self.expr(t)?;
                    }
                    if let Some(u) = update {
                        self.expr(u)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                r
            }
            Stmt::ForInOf {
                decl,
                left,
                right,
                body,
                ..
            } => {
                self.expr(right)?;
                let mut names = std::collections::HashSet::new();
                match decl {
                    Some(DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing) => {
                        pat_idents(left, &mut names);
                    }
                    Some(DeclKind::Var) => {} // already in the hoisted set
                    None => {}
                }
                self.push_scope(names);
                let r = (|| {
                    if decl.is_none() {
                        self.pat_targets(left)?;
                    } else {
                        self.pat_decl_exprs(left)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                r
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => Some(()),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.block(block)?;
                if let Some((param, body)) = handler {
                    let mut names = std::collections::HashSet::new();
                    if let Some(p) = param {
                        pat_idents(p, &mut names);
                    }
                    self.declare_lexicals(body, &mut names);
                    self.push_scope(names);
                    let r = (|| {
                        if let Some(p) = param {
                            self.pat_decl_exprs(p)?;
                        }
                        for s in body {
                            self.stmt(s)?;
                        }
                        Some(())
                    })();
                    self.scopes.pop();
                    r?;
                }
                if let Some(f) = finalizer {
                    self.block(f)?;
                }
                Some(())
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc)?;
                let mut names = std::collections::HashSet::new();
                for c in cases {
                    self.declare_lexicals(&c.body, &mut names);
                }
                self.push_scope(names);
                let r = (|| {
                    for c in cases {
                        if let Some(t) = &c.test {
                            self.expr(t)?;
                        }
                        for s in &c.body {
                            self.stmt(s)?;
                        }
                    }
                    Some(())
                })();
                self.scopes.pop();
                r
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::ClassDecl(c) => self.class(c),
            // `with`, modules, and anything else unrecognized: unanalyzable.
            _ => None,
        }
    }

    /// Enter an inner function (declaration, expression, method, accessor…).
    fn inner_fn(&mut self, f: &Function) -> Option<()> {
        self.fn_depth += 1;
        self.arrow_path.push(f.is_arrow);
        let r = self.fn_body(f);
        self.arrow_path.pop();
        self.fn_depth -= 1;
        r
    }

    fn class(&mut self, c: &Class) -> Option<()> {
        // Heritage, decorators, and computed keys evaluate at definition time (current depth);
        // method bodies / field initializers / static blocks run later (inner-function depth).
        for d in &c.decorators {
            self.expr(d)?;
        }
        if let Some(sc) = &c.superclass {
            self.expr(sc)?;
        }
        let mut names = std::collections::HashSet::new();
        if let Some(n) = &c.name {
            names.insert(n.clone());
        }
        self.push_scope(names);
        let r = (|| {
            for m in &c.members {
                for d in &m.decorators {
                    self.expr(d)?;
                }
                if let PropKey::Computed(k) = &m.key {
                    self.expr(k)?;
                }
                if let Some(f) = &m.func {
                    self.inner_fn(f)?;
                }
                if let Some(v) = &m.value {
                    // Field initializers run in an implicit method with its own `this` (the
                    // instance) — inner depth, and NOT part of any outer arrow chain.
                    self.fn_depth += 1;
                    self.arrow_path.push(false);
                    let r = self.expr(v);
                    self.arrow_path.pop();
                    self.fn_depth -= 1;
                    r?;
                }
            }
            Some(())
        })();
        self.scopes.pop();
        r
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Regex { .. }
            | Expr::Super
            | Expr::NewTarget
            | Expr::ImportMeta => Some(()),
            Expr::This => {
                // `this` read through an unbroken arrow chain from the outer function observes
                // the outer `this` — the activation must carry it.
                if self.fn_depth > 0 && self.arrow_path[1..].iter().all(|a| *a) {
                    self.env_this = true;
                }
                Some(())
            }
            Expr::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Expr::Paren(i) | Expr::ToStr(i) | Expr::Await(i) | Expr::OptionalChain(i) => {
                self.expr(i)
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Object(props) => {
                for p in props {
                    match p {
                        PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.expr(value)?;
                        }
                        PropDef::Method { key, func }
                        | PropDef::Getter { key, func }
                        | PropDef::Setter { key, func } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.inner_fn(func)?;
                        }
                        PropDef::Spread(e) | PropDef::Proto(e) => self.expr(e)?,
                    }
                }
                Some(())
            }
            Expr::Func(f) => self.inner_fn(f),
            Expr::Class(c) => self.class(c),
            Expr::Yield { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a)?;
                }
                Some(())
            }
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => self.expr(arg),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Assign { target, value, .. } => {
                // A destructuring assignment target is a pattern of references.
                match &**target {
                    Expr::Array(_) | Expr::Object(_) => {
                        // Reinterpreting the literal as a pattern is the parser's job; walking it
                        // as an expression visits the same identifiers (Cover handles defaults).
                        self.expr(target)?;
                    }
                    t => self.expr(t)?,
                }
                self.expr(value)
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                self.expr(cons)?;
                self.expr(alt)
            }
            Expr::Call { callee, args, .. } => {
                // Direct eval inside any nested function could name arbitrary outer locals.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return None;
                }
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Member { obj, .. } => self.expr(obj),
            Expr::Index { obj, index, .. } => {
                self.expr(obj)?;
                self.expr(index)
            }
            Expr::Seq(es) => {
                for e in es {
                    self.expr(e)?;
                }
                Some(())
            }
            Expr::TaggedTemplate { tag, subs, .. } => {
                self.expr(tag)?;
                for s in subs {
                    self.expr(s)?;
                }
                Some(())
            }
            Expr::PrivateIn { obj, .. } => self.expr(obj),
            Expr::ImportCall { spec, options, .. } => {
                self.expr(spec)?;
                if let Some(o) = options {
                    self.expr(o)?;
                }
                Some(())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

/// Compile `func` whole, or `None` if it uses anything outside the v0 subset.
pub fn compile(func: &Function) -> Option<Rc<Chunk>> {
    // Body facts the scanner already knows: `arguments` / `new.target` are observation channels
    // into the activation that slots do not provide; `this` in an arrow is a free variable we
    // do not model.
    let scan = func.scan_flags();
    if scan & SCAN_ARGUMENTS != 0 || scan & SCAN_NEW_TARGET != 0 {
        return None;
    }
    if func.is_arrow && scan & SCAN_THIS != 0 {
        return None;
    }
    // Generators still run in the tree-walker (their `.return()`/`.throw()` injection and yield*
    // delegation are not modeled here). Async functions compile: `await` lowers to `Op::Await`,
    // which suspends the `VmCoro` that drives this body.
    if func.is_generator {
        return None;
    }
    // A named function expression binds its own name inside the body — an env-side binding the
    // slot model doesn't carry.
    if func.is_fn_expr && func.name.is_some() {
        return None;
    }
    // Capture analysis: which locals inner functions can name (they live in a real activation
    // env), and whether an inner arrow chain reads `this`. `None` = unanalyzable — bail.
    let (captured, env_this) = CaptureScan::run(func)?;

    let mut c = Compiler {
        env_this,
        ..Compiler::default()
    };
    // Parameters: plain identifiers only, one positional slot each (a sloppy duplicate name
    // resolves to the later parameter, matching the env behavior where the later insert wins).
    // A captured parameter keeps its positional slot (dead) but homes in the activation env.
    for (k, p) in func.params.iter().enumerate() {
        if p.default.is_some() || p.rest {
            return None;
        }
        let Pattern::Ident(name) = &p.pattern else {
            return None;
        };
        let slot = c.fresh_slot(name);
        if captured.contains(name) {
            c.cap_inits.push(CapInit::Param(k as u16, Rc::from(name.as_str())));
            c.env_bind(name, false);
        } else {
            c.scope_bind(name, slot, false);
        }
    }
    c.n_params = func.params.len();
    // Function-scoped `var`s and hoisted function declarations from the shared hoist plan.
    for op in crate::interpreter::collect_hoist_ops(&func.body, func.is_strict, &[]) {
        match op {
            HoistOp::Var(name) => {
                if captured.contains(&name) {
                    if !c.env_has(&name) {
                        c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                        c.env_bind(&name, false);
                    }
                } else if c.lookup(&name).is_none() {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                }
            }
            HoistOp::VarForce(name) => {
                if captured.contains(&name) {
                    return None; // for-head reset of a captured param — stay in the oracle
                }
                let slot = match c.lookup(&name) {
                    Some((s, _)) => s,
                    None => {
                        let s = c.fresh_slot(&name);
                        c.scope_bind(&name, s, false);
                        s
                    }
                };
                if (slot as usize) < func.params.len() {
                    c.var_force_resets.push(slot);
                }
            }
            HoistOp::Fn(name, f) => {
                let fidx = c.funcs.len() as u16;
                c.funcs.push(f.clone());
                if captured.contains(&name) {
                    c.cap_inits.push(CapInit::Fn(fidx, Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                } else {
                    let slot = match c.lookup(&name) {
                        Some((s, _)) => s,
                        None => {
                            let s = c.fresh_slot(&name);
                            c.scope_bind(&name, s, false);
                            s
                        }
                    };
                    // Created at entry, in hoist order, closing over the activation.
                    c.emit(Op::MakeClosure(fidx as u32, u32::MAX));
                    c.emit(Op::StoreLocal(slot));
                }
            }
            // Annex B promotions have declaration-time sync the VM doesn't model — bail.
            HoistOp::AnnexB(..) => return None,
        }
    }
    // Body-level lexicals: captured ones home in the activation (inserted in TDZ by
    // make_run_env), the rest get TDZ slots.
    c.declare_body_lexicals(&func.body, &captured).ok()?;
    for stmt in &func.body {
        c.stmt(stmt).ok()?;
    }
    c.emit(Op::ReturnUndef);
    Some(Rc::new(Chunk {
        ops: c.ops,
        consts: c.consts,
        names: c.names,
        n_slots: c.slot_names.len(),
        slot_names: c.slot_names,
        n_params: c.n_params,
        var_force_resets: c.var_force_resets,
        uses_this: c.uses_this,
        funcs: c.funcs,
        cap_inits: c.cap_inits,
        env_this: c.env_this,
        caches: c.caches,
        name_pins: std::cell::RefCell::new(vec![None; c.name_caches.len()]),
        name_caches: c.name_caches,
        jit: std::cell::OnceCell::new(),
    }))
}

#[derive(Default)]
struct Compiler {
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    /// Lexical scopes for slot resolution: (name, slot, is_const), innermost last.
    scopes: Vec<Vec<(String, u16, bool)>>,
    slot_names: Vec<Rc<str>>,
    n_params: usize,
    var_force_resets: Vec<u16>,
    loops: Vec<LoopCtx>,
    /// Labels collected from an enclosing `Stmt::Labeled` chain, waiting to be attached to the next
    /// loop's `LoopCtx` (drained when that loop pushes its context).
    pending_labels: Vec<String>,
    uses_this: bool,
    caches: Vec<std::cell::Cell<IcState>>,
    name_caches: Vec<std::cell::Cell<NameIc>>,
    /// Captured (env-homed) function-scope-wide names → is_const. Slot scopes shadow these.
    env_names: std::collections::HashMap<String, bool>,
    funcs: Vec<Rc<Function>>,
    cap_inits: Vec<CapInit>,
    env_this: bool,
}

/// Where a name resolves inside the compiled body.
enum Home {
    Slot(u16, bool),
    /// Captured: lives in the activation env; bool = is_const.
    Env(bool),
}

#[derive(Default)]
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// Labels naming this loop (usually zero or one; `a: b: for(…)` stacks several). A labelled
    /// `break`/`continue` searches the loop stack for the ctx carrying its target label.
    labels: Vec<String>,
    /// A `switch` context: an unlabelled `break` targets it, but `continue` skips past it to the
    /// innermost enclosing loop.
    is_switch: bool,
}

/// Compilation bail: the construct is outside the v0 subset.
struct Bail;
type CResult = Result<(), Bail>;

/// Whether `e` provably cannot reassign the local `name` (for fused element ops, which defer the
/// base-slot read past this expression's evaluation). Whitelist recursion: any variant not
/// explicitly handled answers `false` (don't fuse). Calls and nested functions are safe — a slot
/// local is unobservable outside its function (that is what makes slot storage sound), so only a
/// syntactic assignment/update in this very expression could touch it.
fn no_assign_to(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(_)
        | Expr::This
        | Expr::Regex { .. }
        | Expr::Func(_) => true,
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => no_assign_to(x, name),
        Expr::Update { arg, .. } => match &**arg {
            Expr::Ident(n) => n != name,
            Expr::Member { obj, .. } => no_assign_to(obj, name),
            Expr::Index { obj, index, .. } => {
                no_assign_to(obj, name) && no_assign_to(index, name)
            }
            _ => false,
        },
        Expr::Assign { target, value, .. } => {
            let target_ok = match &**target {
                Expr::Ident(n) => n != name,
                Expr::Member { obj, .. } => no_assign_to(obj, name),
                Expr::Index { obj, index, .. } => {
                    no_assign_to(obj, name) && no_assign_to(index, name)
                }
                _ => false, // destructuring pattern — could bind `name`
            };
            target_ok && no_assign_to(value, name)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            no_assign_to(left, name) && no_assign_to(right, name)
        }
        Expr::Cond { test, cons, alt } => {
            no_assign_to(test, name) && no_assign_to(cons, name) && no_assign_to(alt, name)
        }
        Expr::Member { obj, .. } => no_assign_to(obj, name),
        Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            no_assign_to(callee, name)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
            ArrayElem::Hole => true,
        }),
        _ => false,
    }
}

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }
    /// Reserve a fresh inline-cache slot (starts empty) for a property-access op.
    fn new_cache(&mut self) -> u32 {
        self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        (self.caches.len() - 1) as u32
    }
    /// Reserve a fresh name-cache slot for a free-name op.
    fn new_name_cache(&mut self) -> u32 {
        self.name_caches.push(std::cell::Cell::new(NameIc::EMPTY));
        (self.name_caches.len() - 1) as u32
    }
    /// The parameter slot for a fused element access (`x[k]` → `GetElemLocal`), or `None` to use
    /// the generic ops. Fusing defers the base-local read past the key/value evaluation, so it
    /// requires: the base is an Ident homed in a *parameter* slot (never TDZ — no early throw to
    /// reorder), and no `deps` expression can reassign that local (calls can't — slot locals are
    /// unobservable outside the function; only an explicit assignment/update in the key/value
    /// expressions themselves could, and `no_assign_to` rejects those).
    fn fused_elem_slot(&self, obj: &Expr, deps: &[&Expr]) -> Option<u16> {
        let Expr::Ident(name) = obj else { return None };
        let Some(Home::Slot(slot, _)) = self.home(name) else {
            return None;
        };
        if (slot as usize) >= self.n_params {
            return None;
        }
        if deps.iter().all(|d| no_assign_to(d, name)) {
            Some(slot)
        } else {
            None
        }
    }
    fn fresh_slot(&mut self, name: &str) -> u16 {
        let slot = self.slot_names.len() as u16;
        self.slot_names.push(Rc::from(name));
        slot
    }
    fn scope_bind(&mut self, name: &str, slot: u16, is_const: bool) {
        if self.scopes.is_empty() {
            self.scopes.push(Vec::new());
        }
        let top = self.scopes.last_mut().unwrap();
        if let Some(e) = top.iter_mut().find(|(n, ..)| n == name) {
            *e = (name.to_string(), slot, is_const);
        } else {
            top.push((name.to_string(), slot, is_const));
        }
    }
    fn lookup(&self, name: &str) -> Option<(u16, bool)> {
        for scope in self.scopes.iter().rev() {
            if let Some((_, slot, k)) = scope.iter().rev().find(|(n, ..)| n == name) {
                return Some((*slot, *k));
            }
        }
        None
    }
    fn env_bind(&mut self, name: &str, is_const: bool) {
        self.env_names.insert(name.to_string(), is_const);
    }
    fn env_has(&self, name: &str) -> bool {
        self.env_names.contains_key(name)
    }
    /// Resolve a local: innermost slot scope first (block lexicals shadow captured names — a
    /// captured block lexical bails compile, so every env name is function-scope-wide).
    fn home(&self, name: &str) -> Option<Home> {
        if let Some((slot, k)) = self.lookup(name) {
            return Some(Home::Slot(slot, k));
        }
        self.env_names.get(name).map(|k| Home::Env(*k))
    }
    fn const_idx(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }
    fn name_idx(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return i as u32;
        }
        self.names.push(Rc::from(name));
        (self.names.len() - 1) as u32
    }
    fn patch(&mut self, at: usize) {
        let target = self.ops.len() as u32;
        match &mut self.ops[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t) => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }

    /// Declare the function body's top-level `let`/`const`: captured ones home in the activation
    /// env (inserted in TDZ by `make_run_env`), the rest get TDZ slots. Function declarations
    /// were already handled by the hoist plan; classes and `using` bail.
    fn declare_body_lexicals(
        &mut self,
        stmts: &[Stmt],
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: kind @ (DeclKind::Let | DeclKind::Const),
                    decls,
                } => {
                    for (pat, _) in decls {
                        let Pattern::Ident(name) = pat else {
                            return Err(Bail);
                        };
                        let is_const = matches!(kind, DeclKind::Const);
                        if captured.contains(name) {
                            self.cap_inits
                                .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                            self.env_bind(name, is_const);
                        } else {
                            let slot = self.fresh_slot(name);
                            self.scope_bind(name, slot, is_const);
                            self.emit(Op::Tdz(slot));
                        }
                    }
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                }
                | Stmt::ClassDecl(_) => return Err(Bail),
                Stmt::FuncDecl(_) => {} // hoisted — created at entry
                _ => {}
            }
        }
        Ok(())
    }

    /// Declare a statement list's `let`/`const` as TDZ slots (block entry).
    fn declare_block_lexicals(&mut self, stmts: &[Stmt]) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                } => {
                    for (pat, _) in decls {
                        let Pattern::Ident(name) = pat else {
                            return Err(Bail);
                        };
                        let slot = self.fresh_slot(name);
                        self.scope_bind(
                            name,
                            slot,
                            matches!(
                                s,
                                Stmt::VarDecl {
                                    kind: DeclKind::Const,
                                    ..
                                }
                            ),
                        );
                        self.emit(Op::Tdz(slot));
                    }
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                }
                | Stmt::ClassDecl(_)
                | Stmt::FuncDecl(_) => return Err(Bail),
                _ => {}
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> CResult {
        match s {
            Stmt::Expr(e) => self.expr_stmt(e),
            Stmt::Empty | Stmt::Debugger => Ok(()),
            // Top-level function declarations were hoisted (created at entry); block-level ones
            // never reach here (declare_block_lexicals bails first).
            Stmt::FuncDecl(_) => Ok(()),
            Stmt::VarDecl { kind, decls } => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                for (pat, init) in decls {
                    let Pattern::Ident(name) = pat else {
                        return Err(Bail);
                    };
                    let home = self.home(name).ok_or(Bail)?;
                    match init {
                        Some(e) => self.named_expr(e, name)?,
                        // `var x;` leaves an existing binding alone; `let x;` initializes.
                        None => {
                            if matches!(kind, DeclKind::Var) {
                                continue;
                            }
                            self.emit(Op::Undef);
                        }
                    }
                    match home {
                        Home::Slot(slot, _) => {
                            self.emit(Op::StoreLocal(slot));
                        }
                        Home::Env(_) => {
                            let n = self.name_idx(name);
                            // A lexical declaration initializes (clearing TDZ); a `var` writes an
                            // already-initialized binding.
                            if matches!(kind, DeclKind::Var) {
                                self.emit(Op::StoreCap(n));
                            } else {
                                self.emit(Op::StoreCapInit(n));
                            }
                        }
                    }
                }
                Ok(())
            }
            Stmt::Return(arg) => {
                match arg {
                    Some(e) => {
                        self.expr(e)?;
                        self.emit(Op::Return);
                    }
                    None => {
                        self.emit(Op::ReturnUndef);
                    }
                }
                Ok(())
            }
            Stmt::Throw(e) => {
                self.expr(e)?;
                self.emit(Op::Throw);
                Ok(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.stmt(cons)?;
                match alt {
                    Some(a) => {
                        let jend = self.emit(Op::Jump(0));
                        self.patch(jf);
                        self.stmt(a)?;
                        self.patch(jend);
                    }
                    None => self.patch(jf),
                }
                Ok(())
            }
            Stmt::Block(body) => {
                self.scopes.push(Vec::new());
                let r = self.block_body(body);
                self.scopes.pop();
                r
            }
            Stmt::While { test, body } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.loops.push(LoopCtx {
                    labels,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = start as u32,
                        _ => unreachable!(),
                    }
                }
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::DoWhile { body, test } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.loops.push(LoopCtx {
                    labels,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                let cont = self.ops.len();
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = cont as u32,
                        _ => unreachable!(),
                    }
                }
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.scopes.push(Vec::new());
                let r = self.for_loop(init.as_deref(), test.as_ref(), update.as_ref(), body);
                self.scopes.pop();
                r
            }
            Stmt::Break(None) => {
                let j = self.emit(Op::Jump(0));
                self.loops.last_mut().ok_or(Bail)?.breaks.push(j);
                Ok(())
            }
            Stmt::Continue(None) => {
                let j = self.emit(Op::Jump(0));
                // `continue` skips switch contexts: it targets the innermost enclosing *loop*.
                self.loops
                    .iter_mut()
                    .rev()
                    .find(|c| !c.is_switch)
                    .ok_or(Bail)?
                    .continues
                    .push(j);
                Ok(())
            }
            // Labelled break/continue: jump to the loop on the stack that carries the target label.
            // A `break` to a labelled *block* (not a loop) isn't modeled here — no ctx matches, so
            // it bails to the interpreter.
            Stmt::Break(Some(name)) => {
                let j = self.emit(Op::Jump(0));
                self.labeled_loop_mut(name).ok_or(Bail)?.breaks.push(j);
                Ok(())
            }
            Stmt::Continue(Some(name)) => {
                let j = self.emit(Op::Jump(0));
                // A labelled continue must target a loop — a label on a switch is only a break
                // target (the parser rejects `continue` to it; not-found bails to the oracle).
                self.loops
                    .iter_mut()
                    .rev()
                    .find(|c| !c.is_switch && c.labels.iter().any(|l| l == name))
                    .ok_or(Bail)?
                    .continues
                    .push(j);
                Ok(())
            }
            // A label naming a loop or switch attaches to that context; stacked labels
            // (`a: b: for`) accumulate through the recursion. A label on any other statement bails.
            Stmt::Labeled { label, body } => match &**body {
                Stmt::While { .. }
                | Stmt::DoWhile { .. }
                | Stmt::For { .. }
                | Stmt::Switch { .. }
                | Stmt::Labeled { .. } => {
                    self.pending_labels.push(label.clone());
                    self.stmt(body)
                }
                _ => Err(Bail),
            },
            // `switch`: the discriminant lands in a hidden slot; case tests run in source order
            // (exactly the oracle's two-phase evaluation), then bodies are laid out contiguously
            // so fall-through is just falling through. Any lexical/class/function declaration
            // directly in a case body bails — the oracle gives all cases one shared block scope
            // whose TDZ interleavings slots don't model.
            Stmt::Switch { disc, cases } => {
                for case in cases {
                    for s in &case.body {
                        match s {
                            Stmt::VarDecl {
                                kind:
                                    DeclKind::Let
                                    | DeclKind::Const
                                    | DeclKind::Using
                                    | DeclKind::AwaitUsing,
                                ..
                            }
                            | Stmt::ClassDecl(_)
                            | Stmt::FuncDecl(_) => return Err(Bail),
                            _ => {}
                        }
                    }
                }
                self.expr(disc)?;
                let tmp = self.fresh_slot("%switch%");
                self.emit(Op::StoreLocal(tmp));
                // Phase 1: the test chain. Each match jumps to its (not yet emitted) body.
                let mut body_jumps: Vec<(usize, usize)> = Vec::new();
                for (ci, case) in cases.iter().enumerate() {
                    if let Some(test) = &case.test {
                        self.emit(Op::LoadLocal(tmp));
                        self.expr(test)?;
                        self.emit(Op::StrictEq);
                        let jf = self.emit(Op::JumpIfFalse(0));
                        let jb = self.emit(Op::Jump(0));
                        body_jumps.push((ci, jb));
                        self.patch(jf);
                    }
                }
                let jdefault = self.emit(Op::Jump(0));
                self.loops.push(LoopCtx {
                    labels: std::mem::take(&mut self.pending_labels),
                    is_switch: true,
                    ..LoopCtx::default()
                });
                // Phase 2: bodies, contiguous and in source order.
                let mut body_starts = vec![0usize; cases.len()];
                let mut r = Ok(());
                'bodies: for (ci, case) in cases.iter().enumerate() {
                    body_starts[ci] = self.ops.len();
                    for s in &case.body {
                        r = self.stmt(s);
                        if r.is_err() {
                            break 'bodies;
                        }
                    }
                }
                let ctx = self.loops.pop().unwrap();
                r?;
                for (ci, at) in body_jumps {
                    match &mut self.ops[at] {
                        Op::Jump(t) => *t = body_starts[ci] as u32,
                        _ => unreachable!(),
                    }
                }
                match cases.iter().position(|c| c.test.is_none()) {
                    Some(di) => match &mut self.ops[jdefault] {
                        Op::Jump(t) => *t = body_starts[di] as u32,
                        _ => unreachable!(),
                    },
                    None => self.patch(jdefault),
                }
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            // `try { ... } catch (e?) { ... }` — no `finally` (bails), catch param an ident or none.
            // On a throw in the try region the VM unwinds to `catch_pc` with the exception pushed.
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                if finalizer.is_some() {
                    return Err(Bail); // `finally` is not modeled in the VM yet
                }
                let Some((param, catch_body)) = handler else {
                    return Err(Bail); // `try`/`finally` with no `catch`
                };
                if matches!(param, Some(p) if !matches!(p, Pattern::Ident(_))) {
                    return Err(Bail); // destructuring catch param
                }
                let push = self.emit(Op::PushHandler(0));
                self.scopes.push(Vec::new());
                let tr = self.block_body(block);
                self.scopes.pop();
                tr?;
                self.emit(Op::PopHandler);
                let jmp_after = self.emit(Op::Jump(0));
                // Catch entry: the exception is on the stack.
                let catch_pc = self.ops.len() as u32;
                match &mut self.ops[push] {
                    Op::PushHandler(t) => *t = catch_pc,
                    _ => unreachable!(),
                }
                self.scopes.push(Vec::new());
                match param {
                    Some(Pattern::Ident(name)) => {
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, false);
                        self.emit(Op::StoreLocal(slot));
                    }
                    _ => {
                        self.emit(Op::Pop); // no binding (or `catch {}`): discard the exception
                    }
                }
                let cr = self.block_body(catch_body);
                self.scopes.pop();
                cr?;
                self.patch(jmp_after);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// The nearest enclosing loop context labelled `name`, searched innermost-first.
    fn labeled_loop_mut(&mut self, name: &str) -> Option<&mut LoopCtx> {
        self.loops
            .iter_mut()
            .rev()
            .find(|c| c.labels.iter().any(|l| l == name))
    }

    fn block_body(&mut self, body: &[Stmt]) -> CResult {
        self.declare_block_lexicals(body)?;
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn for_loop(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> CResult {
        // Claim any labels from an enclosing `Stmt::Labeled` before the head runs, so they land on
        // this loop's context (the head itself introduces no labelled break/continue targets).
        let labels = std::mem::take(&mut self.pending_labels);
        match init {
            Some(ForInit::VarDecl { kind, decls }) => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                if matches!(kind, DeclKind::Let | DeclKind::Const) {
                    for (pat, _) in decls {
                        let Pattern::Ident(name) = pat else {
                            return Err(Bail);
                        };
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, matches!(kind, DeclKind::Const));
                        self.emit(Op::Tdz(slot));
                    }
                }
                for (pat, initv) in decls {
                    let Pattern::Ident(name) = pat else {
                        return Err(Bail);
                    };
                    let (slot, _) = self.lookup(name).ok_or(Bail)?;
                    match initv {
                        Some(e) => {
                            self.expr(e)?;
                            self.emit(Op::StoreLocal(slot));
                        }
                        None => {
                            if !matches!(kind, DeclKind::Var) {
                                self.emit(Op::Undef);
                                self.emit(Op::StoreLocal(slot));
                            }
                        }
                    }
                }
            }
            Some(ForInit::Expr(e)) => {
                self.expr_stmt(e)?;
            }
            None => {}
        }
        let start = self.ops.len();
        let jf = match test {
            Some(t) => {
                self.expr(t)?;
                Some(self.emit(Op::JumpIfFalse(0)))
            }
            None => None,
        };
        self.loops.push(LoopCtx {
            labels,
            ..LoopCtx::default()
        });
        let r = self.stmt(body);
        let ctx = self.loops.pop().unwrap();
        r?;
        let cont = self.ops.len();
        for c in ctx.continues {
            match &mut self.ops[c] {
                Op::Jump(t) => *t = cont as u32,
                _ => unreachable!(),
            }
        }
        if let Some(u) = update {
            self.expr_stmt(u)?;
        }
        self.emit(Op::Jump(start as u32));
        if let Some(jf) = jf {
            self.patch(jf);
        }
        for b in ctx.breaks {
            self.patch(b);
        }
        Ok(())
    }

    /// Compile an expression whose value is discarded (an expression statement, or a `for`
    /// header's init / update). Assignments and `++`/`--` to a local drop their producing `Dup`
    /// (and the trailing `Pop`); everything else falls back to `expr` + `Pop`. Semantically
    /// identical to `self.expr(e)?; self.emit(Op::Pop)` — the only difference is the unobservable
    /// result value.
    fn expr_stmt(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Paren(inner) => return self.expr_stmt(inner),
            // A comma expression as a statement: every operand is evaluated for effect only.
            Expr::Seq(exprs) => {
                for ex in exprs {
                    self.expr_stmt(ex)?;
                }
                return Ok(());
            }
            Expr::Update { op, arg, .. } => {
                let kind = match *op {
                    "++" => UpdKind::IncDiscard,
                    "--" => UpdKind::DecDiscard,
                    _ => return Err(Bail),
                };
                return self.update_target(arg, kind);
            }
            Expr::Assign { op, target, value } => {
                return self.assign_discard(op, target, value);
            }
            _ => {}
        }
        self.expr(e)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Compile a discarded assignment: the fast `Dup`-free lowering when the target is a plain
    /// local / free name / `obj.x` / `obj[k]`, otherwise the generic value-producing `assign`
    /// followed by `Pop` (identical to `self.expr(assign)?; Pop`).
    fn assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if self.try_assign_discard(op, target, value)? {
            return Ok(());
        }
        self.assign(op, target, value)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Fast lowering for a discarded assignment (no `Dup`, no trailing `Pop`). Returns `Ok(true)`
    /// when it emitted the assignment, `Ok(false)` to defer to the generic `assign` + `Pop` path
    /// (which handles — or itself bails on — the forms not covered here). Any `Bail` from a
    /// compiled sub-expression propagates: the generic path would bail identically.
    fn try_assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> Result<bool, Bail> {
        // Logical-assignment short-circuits; leave it to the generic path (which bails).
        if matches!(op, "&&=" | "||=" | "??=") {
            return Ok(false);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        return Ok(false);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreLocal(slot));
                    Ok(true)
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Ok(false); // runtime TypeError — the oracle's business
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreCap(n));
                    Ok(true)
                }
                None => {
                    if op != "=" {
                        return Ok(false);
                    }
                    // StoreName already consumes the value without re-pushing it.
                    self.named_expr(value, name)?;
                    let i = self.name_idx(name);
                    self.emit(Op::StoreName(i));
                    Ok(true)
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetPropDrop(i, c));
                Ok(true)
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if op == "=" {
                    if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                        self.expr(index)?;
                        self.expr(value)?;
                        self.emit(Op::SetElemLocalDrop(slot));
                        return Ok(true);
                    }
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElemDrop);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Emit a closure over the current environment; `name` applies NamedEvaluation to an
    /// anonymous function expression (`var f = function(){}` → `f.name === "f"`).
    fn emit_closure(&mut self, f: &Rc<Function>, name: Option<&str>) {
        let fidx = self.funcs.len() as u32;
        self.funcs.push(f.clone());
        let name_idx = match name {
            Some(n) if f.name.is_none() && !f.is_method => self.name_idx(n),
            _ => u32::MAX,
        };
        self.emit(Op::MakeClosure(fidx, name_idx));
    }

    /// Compile a value expression in a naming position (declaration/assignment to `name`).
    fn named_expr(&mut self, e: &Expr, name: &str) -> CResult {
        if let Expr::Func(f) = e {
            self.emit_closure(f, Some(name));
            return Ok(());
        }
        self.expr(e)
    }

    fn expr(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Func(f) => {
                self.emit_closure(f, None);
                Ok(())
            }
            Expr::Num(n) => {
                let i = self.const_idx(Value::Num(*n));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Str(s) => {
                let i = self.const_idx(Value::Str(s.clone()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Bool(b) => {
                let i = self.const_idx(Value::Bool(*b));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Null => {
                let i = self.const_idx(Value::Null);
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Undefined => {
                self.emit(Op::Undef);
                Ok(())
            }
            Expr::BigInt(n) => {
                let i = self.const_idx(Value::BigInt(n.clone()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Ident(name) => {
                match self.home(name) {
                    Some(Home::Slot(slot, _)) => {
                        self.emit(Op::LoadLocal(slot));
                    }
                    Some(Home::Env(_)) => {
                        let i = self.name_idx(name);
                        self.emit(Op::LoadCap(i));
                    }
                    None => {
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                    }
                };
                Ok(())
            }
            Expr::This => {
                self.uses_this = true;
                self.emit(Op::LoadThis);
                Ok(())
            }
            Expr::Paren(inner) => self.expr(inner),
            Expr::Seq(exprs) => {
                for (k, ex) in exprs.iter().enumerate() {
                    self.expr(ex)?;
                    if k + 1 < exprs.len() {
                        self.emit(Op::Pop);
                    }
                }
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref()]) {
                    self.expr(index)?;
                    self.emit(Op::GetElemLocal(slot));
                } else {
                    self.expr(obj)?;
                    self.expr(index)?;
                    self.emit(Op::GetElem);
                }
                Ok(())
            }
            Expr::Binary { op, left, right } => {
                self.expr(left)?;
                self.expr(right)?;
                let bop = match *op {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "&" => Op::BitAnd,
                    "|" => Op::BitOr,
                    "^" => Op::BitXor,
                    "<<" => Op::Shl,
                    ">>" => Op::Shr,
                    ">>>" => Op::UShr,
                    "<" => Op::Lt,
                    ">" => Op::Gt,
                    "<=" => Op::Le,
                    ">=" => Op::Ge,
                    "==" => Op::EqEq,
                    "!=" => Op::NotEq,
                    "===" => Op::StrictEq,
                    "!==" => Op::StrictNotEq,
                    other => {
                        let i = self.name_idx(other);
                        Op::GenBin(i)
                    }
                };
                self.emit(bop);
                Ok(())
            }
            Expr::Logical { op, left, right } => {
                self.expr(left)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalsePeek(0)),
                    "||" => self.emit(Op::JumpIfTruePeek(0)),
                    "??" => self.emit(Op::JumpIfNotNullishPeek(0)),
                    _ => return Err(Bail),
                };
                self.emit(Op::Pop);
                self.expr(right)?;
                self.patch(j);
                Ok(())
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.expr(cons)?;
                let jend = self.emit(Op::Jump(0));
                self.patch(jf);
                self.expr(alt)?;
                self.patch(jend);
                Ok(())
            }
            Expr::Unary { op, arg } => {
                match *op {
                    "-" => {
                        self.expr(arg)?;
                        self.emit(Op::Neg);
                    }
                    "+" => {
                        self.expr(arg)?;
                        self.emit(Op::Plus);
                    }
                    "!" => {
                        self.expr(arg)?;
                        self.emit(Op::Not);
                    }
                    "~" => {
                        self.expr(arg)?;
                        self.emit(Op::BitNot);
                    }
                    "void" => {
                        self.expr(arg)?;
                        self.emit(Op::Void);
                    }
                    "typeof" => {
                        // `typeof freeName` must not throw on unresolved names — that path
                        // stays in the oracle.
                        if matches!(&**arg, Expr::Ident(n) if self.home(n).is_none()) {
                            return Err(Bail);
                        }
                        self.expr(arg)?;
                        self.emit(Op::Typeof);
                    }
                    _ => return Err(Bail),
                }
                Ok(())
            }
            Expr::Await(arg) => {
                self.expr(arg)?;
                self.emit(Op::Await);
                Ok(())
            }
            Expr::Update { op, prefix, arg } => {
                let kind = match (*op, *prefix) {
                    ("++", true) => UpdKind::PreInc,
                    ("--", true) => UpdKind::PreDec,
                    ("++", false) => UpdKind::PostInc,
                    ("--", false) => UpdKind::PostDec,
                    _ => return Err(Bail),
                };
                self.update_target(arg, kind)
            }
            Expr::Assign { op, target, value } => self.assign(op, target, value),
            Expr::Call {
                callee,
                args,
                optional: false,
            } => {
                // Direct eval can see the activation — bail the function.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return Err(Bail);
                }
                match &**callee {
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                        self.expr(obj)?;
                        let i = self.name_idx(prop);
                        let c = self.new_cache();
                        self.emit(Op::GetMethod(i, c));
                        for a in args {
                            let ArrayElem::Item(a) = a else {
                                return Err(Bail);
                            };
                            self.expr(a)?;
                        }
                        self.emit(Op::CallWithThis(args.len() as u16));
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) => {
                        self.expr(obj)?;
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                        for a in args {
                            let ArrayElem::Item(a) = a else {
                                return Err(Bail);
                            };
                            self.expr(a)?;
                        }
                        self.emit(Op::CallWithThis(args.len() as u16));
                    }
                    Expr::Super => return Err(Bail),
                    Expr::Ident(name) if self.home(name).is_none() => {
                        // Free-name callee: resolved before the arguments (spec order), and a
                        // `with (obj) f()` hit supplies obj as `this`.
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadNameForCall(i, c));
                        for a in args {
                            let ArrayElem::Item(a) = a else {
                                return Err(Bail);
                            };
                            self.expr(a)?;
                        }
                        self.emit(Op::CallWithThis(args.len() as u16));
                    }
                    other => {
                        self.expr(other)?;
                        for a in args {
                            let ArrayElem::Item(a) = a else {
                                return Err(Bail);
                            };
                            self.expr(a)?;
                        }
                        self.emit(Op::Call(args.len() as u16));
                    }
                }
                Ok(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                for a in args {
                    let ArrayElem::Item(a) = a else {
                        return Err(Bail);
                    };
                    self.expr(a)?;
                }
                self.emit(Op::New(args.len() as u16));
                Ok(())
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) => self.expr(e)?,
                        _ => return Err(Bail),
                    }
                }
                self.emit(Op::MakeArray(elems.len() as u16));
                Ok(())
            }
            Expr::Object(props) => {
                let mut count = 0u16;
                // Keys must land contiguously in `names`; values go on the stack in order.
                let mut keys: Vec<String> = Vec::new();
                for p in props {
                    let PropDef::KeyValue { key, value } = p else {
                        return Err(Bail);
                    };
                    let k = match key {
                        PropKey::Ident(k) => k.clone(),
                        PropKey::Str(k) => k.to_string(),
                        _ => return Err(Bail),
                    };
                    // `__proto__:` in literal position sets the prototype, not a property.
                    if k == "__proto__" || k.starts_with('#') {
                        return Err(Bail);
                    }
                    // NamedEvaluation: `{ m: function(){} }` names the anonymous function "m".
                    self.named_expr(value, &k)?;
                    keys.push(k);
                    count += 1;
                }
                // Keys go into `names` only after every value is compiled — value expressions
                // add names of their own, and the key range must stay contiguous.
                let start = self.names.len() as u32;
                for k in &keys {
                    self.names.push(Rc::from(k.as_str()));
                }
                self.emit(Op::MakeObject(start, count));
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// `++`/`--` on a local slot, `obj.name`, or `obj[k]`; `kind` carries pre/post/discard.
    fn update_target(&mut self, arg: &Expr, kind: UpdKind) -> CResult {
        match arg {
            Expr::Paren(inner) => self.update_target(inner, kind),
            Expr::Ident(name) => {
                match self.home(name) {
                    Some(Home::Slot(slot, false)) => {
                        self.emit(Op::UpdateLocal(slot, kind));
                        Ok(())
                    }
                    Some(Home::Env(false)) => {
                        let n = self.name_idx(name);
                        self.emit(Op::UpdateCap(n, kind));
                        Ok(())
                    }
                    // Const targets and free names (global counters) stay in the oracle.
                    _ => Err(Bail),
                }
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::UpdateProp(i, c, kind));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::UpdateElem(kind));
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if matches!(op, "&&=" | "||=" | "??=") {
            return Err(Bail);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    Ok(())
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCap(n));
                    Ok(())
                }
                None => {
                    if op != "=" {
                        return Err(Bail);
                    }
                    self.named_expr(value, name)?;
                    self.emit(Op::Dup);
                    let i = self.name_idx(name);
                    self.emit(Op::StoreName(i));
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: base evaluated once (Dup), get before the RHS — Reference order.
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if op == "=" {
                    if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                        self.expr(index)?;
                        self.expr(value)?;
                        self.emit(Op::SetElemLocal(slot));
                        return Ok(());
                    }
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: coerce a side-effecting key once, then read-modify-write.
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElem);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn emit_compound(&mut self, op: &str) -> CResult {
        let bop = match op {
            "+=" => Op::Add,
            "-=" => Op::Sub,
            "*=" => Op::Mul,
            "/=" => Op::Div,
            "%=" => Op::Mod,
            "&=" => Op::BitAnd,
            "|=" => Op::BitOr,
            "^=" => Op::BitXor,
            "<<=" => Op::Shl,
            ">>=" => Op::Shr,
            ">>>=" => Op::UShr,
            "**=" => {
                let i = self.name_idx("**");
                Op::GenBin(i)
            }
            _ => return Err(Bail),
        };
        self.emit(bop);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------------------------

/// How one run of the VM ended: the body returned a value, suspended at an `await` (async bodies
/// only — see [`VmCoro`]), or — carried as `Err(Abrupt::Throw)` — threw.
pub enum VmStep {
    Done(Value),
    Await(Value),
}

/// Execute a compiled function body. `env` is the root for free-name resolution — the *definition*
/// environment when called leanly (see `Interp::call_user_inner`), since a compiled body has no
/// observable activation. Parameters seed straight into slots; `this_val` is the already-bound
/// `this` (computed only when the body reads it). Synchronous bodies only — an async body runs
/// through [`VmCoro`], which drives [`run_vm`] and can suspend it.
///
/// The slot and operand-stack buffers come from a per-interpreter pool ([`Interp::vm_pool`]) so a
/// hot call tree does not allocate two `Vec`s per call.
pub fn run(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    this_val: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    // Captured locals (and a lexically-read `this`) live in a per-call activation env; slots
    // hold everything else. No captures → the definition env is used directly.
    let env = chunk.make_run_env(i, env, &this_val, args);
    let (mut slots, mut stack) = i.vm_pool.pop().unwrap_or_default();
    let seed = chunk.n_params.min(args.len());
    slots.extend_from_slice(&args[..seed]);
    slots.resize(chunk.n_slots, Value::Undefined);
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    let mut pc = 0usize;
    let mut handlers: Vec<Handler> = Vec::new();
    let r = drive_vm(
        i, chunk, &env, &mut slots, &mut stack, &mut pc, &this_val, &mut handlers, None,
    );
    slots.clear();
    stack.clear();
    if i.vm_pool.len() < 64 {
        i.vm_pool.push((slots, stack));
    }
    match r? {
        VmStep::Done(v) => Ok(v),
        VmStep::Await(_) => unreachable!("a synchronous bytecode function cannot await"),
    }
}

/// Drive the VM through throws: run to `Done`/`Await`, or on an uncaught op throw unwind to the
/// innermost `try` handler (restore its stack depth, push the exception, jump to its catch) and
/// keep going — propagating only when no handler remains. `pending_throw` injects a throw *before*
/// the first step: a rejected `await` resuming inside a `try` (see [`VmCoro::resume`]).
#[allow(clippy::too_many_arguments)]
fn drive_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
    mut pending_throw: Option<Value>,
) -> Result<VmStep, Abrupt> {
    loop {
        let outcome = match pending_throw.take() {
            Some(e) => Err(Abrupt::Throw(e)),
            None => run_vm(i, chunk, env, slots, stack, pc, this_val, handlers),
        };
        match outcome {
            Ok(step) => return Ok(step),
            Err(Abrupt::Throw(e)) => match handlers.pop() {
                Some(h) => {
                    stack.truncate(h.stack_depth);
                    stack.push(e);
                    *pc = h.catch_pc;
                }
                None => return Err(Abrupt::Throw(e)),
            },
            // Return/Break/Continue never escape a compiled body as an Abrupt; propagate defensively.
            Err(other) => return Err(other),
        }
    }
}

/// Run from `*pc` until the body returns (`Done`), suspends at an `await` (`Await`, async bodies
/// only), or throws (`Err(Abrupt::Throw)`, caught by [`drive_vm`]). Operates on borrowed state so an
/// async [`VmCoro`] can save it at a suspension and restore it on resume.
#[allow(clippy::too_many_arguments)]
fn run_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
) -> Result<VmStep, Abrupt> {
    macro_rules! pop {
        () => {
            stack.pop().expect("vm stack underflow")
        };
    }
    loop {
        let op = chunk.ops[*pc];
        *pc += 1;
        match op {
            Op::Const(k) => stack.push(chunk.consts[k as usize].clone()),
            Op::Undef => stack.push(Value::Undefined),
            Op::Dup => {
                let t = stack.last().expect("vm stack underflow").clone();
                stack.push(t);
            }
            Op::Pop => {
                pop!();
            }
            Op::LoadLocal(s) => {
                let v = slots[s as usize].clone();
                if matches!(v, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                stack.push(v);
            }
            Op::StoreLocal(s) => slots[s as usize] = pop!(),
            Op::UpdateLocal(s, kind) => {
                let idx = s as usize;
                match &slots[idx] {
                    // Reading a slot still in its TDZ is the same ReferenceError as LoadLocal.
                    Value::Empty => {
                        return Err(i.throw(
                            "ReferenceError",
                            format!(
                                "cannot access '{}' before initialization",
                                chunk.slot_names[idx]
                            ),
                        ));
                    }
                    // Fast path: a numeric slot updates in place.
                    Value::Num(n) => {
                        let old = *n;
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        slots[idx] = Value::Num(new);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // BigInt updates stay BigInt (ToNumeric, not ToNumber) — never coerced to a
                    // Number and never thrown on like unary `+` would.
                    Value::BigInt(n) => {
                        let old = n.clone();
                        let one = crate::bigint::JsBigInt::from_u64(1);
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old.add(&one),
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old.sub(&one),
                        };
                        slots[idx] = Value::BigInt(new.clone());
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::BigInt(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::BigInt(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // Anything else: ToNumber (may run user `valueOf`), then a Number update. The
                    // post value is the *coerced* number, matching the tree-walker's `eval_update`.
                    _ => {
                        let old = slots[idx].clone();
                        let coerced = i.to_number(&old)?;
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => {
                                coerced + 1.0
                            }
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => {
                                coerced - 1.0
                            }
                        };
                        slots[idx] = Value::Num(new);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(coerced)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                }
            }
            Op::Tdz(s) => slots[s as usize] = Value::Empty,
            Op::LoadCap(n) => {
                let name = &chunk.names[n as usize];
                let b = env.borrow();
                let bd = b.vars.get(&**name).expect("captured binding missing");
                if !bd.initialized {
                    let msg = format!("cannot access '{name}' before initialization");
                    drop(b);
                    return Err(i.throw("ReferenceError", msg));
                }
                let v = bd.value.clone();
                drop(b);
                stack.push(v);
            }
            Op::StoreCap(n) => {
                let name = &chunk.names[n as usize];
                let v = pop!();
                let mut b = env.borrow_mut();
                let bd = b.vars.get_mut(name).expect("captured binding missing");
                if !bd.initialized {
                    let msg = format!("cannot access '{name}' before initialization");
                    drop(b);
                    return Err(i.throw("ReferenceError", msg));
                }
                bd.value = v;
            }
            Op::StoreCapInit(n) => {
                let name = &chunk.names[n as usize];
                let v = pop!();
                let mut b = env.borrow_mut();
                let bd = b.vars.get_mut(name).expect("captured binding missing");
                bd.value = v;
                bd.initialized = true;
            }
            Op::UpdateCap(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = {
                    let b = env.borrow();
                    let bd = b.vars.get(&**name).expect("captured binding missing");
                    if !bd.initialized {
                        let msg = format!("cannot access '{name}' before initialization");
                        drop(b);
                        return Err(i.throw("ReferenceError", msg));
                    }
                    bd.value.clone()
                };
                step_and_store(i, stack, kind, old, |_, v| {
                    if let Some(bd) = env.borrow_mut().vars.get_mut(name) {
                        bd.value = v;
                    }
                    Ok(())
                })?;
            }
            Op::MakeClosure(fidx, name_n) => {
                let v = i.make_function(chunk.funcs[fidx as usize].clone(), env.clone());
                if name_n != u32::MAX {
                    i.set_fn_name(&v, &chunk.names[name_n as usize]);
                }
                stack.push(v);
            }
            Op::LoadName(n, c) => {
                let v = chunk.load_name_ic(i, env, n, c)?;
                stack.push(v);
            }
            Op::StoreName(n) => {
                let v = pop!();
                i.assign_free_name(&chunk.names[n as usize], v, env)?;
            }
            Op::LoadThis => stack.push(this_val.clone()),
            Op::GetProp(n, c) => {
                let obj = pop!();
                let v = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::SetProp(n, c) => {
                let v = pop!();
                let obj = pop!();
                i.set_prop_ic(&obj, &chunk.names[n as usize], v.clone(), &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::SetPropDrop(n, c) => {
                let v = pop!();
                let obj = pop!();
                i.set_prop_ic(&obj, &chunk.names[n as usize], v, &chunk.caches[c as usize])?;
            }
            Op::GetElem => {
                let key = pop!();
                let obj = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
                }
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElem => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    let ret = v.clone();
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => {
                            stack.push(ret);
                            continue;
                        }
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            stack.push(ret);
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v.clone())?;
                stack.push(v);
            }
            Op::SetElemDrop => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::GetElemLocal(s) => {
                let key = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
                }
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
                let keep = matches!(op, Op::SetElemLocal(_));
                let v = pop!();
                let key = pop!();
                if keep {
                    stack.push(v.clone());
                }
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let obj = slots[s as usize].clone();
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let obj = slots[s as usize].clone();
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::UpdateProp(n, c, kind) => {
                let obj = pop!();
                let name = &chunk.names[n as usize];
                let cache = &chunk.caches[c as usize];
                let old = i.get_prop_ic(&obj, name, cache)?;
                step_and_store(i, stack, kind, old, |i, v| {
                    i.set_prop_ic(&obj, name, v, cache)
                })?;
            }
            Op::UpdateElem(kind) => {
                let key = pop!();
                let obj = pop!();
                // Dense-element fast path: numeric key on a plain array/object.
                if let (Value::Obj(o), Value::Num(nk)) = (&obj, &key) {
                    if let Some(Value::Num(old)) = i.fast_get_elem(o, *nk) {
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        if i.fast_set_elem(o, *nk, Value::Num(new)).is_ok() {
                            match kind {
                                UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                                UpdKind::PostInc | UpdKind::PostDec => {
                                    stack.push(Value::Num(old))
                                }
                                UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                            }
                            continue;
                        }
                    }
                }
                // General path: nullish check, one ToPropertyKey, [[Get]], ToNumeric, [[Set]] —
                // the oracle's Reference order exactly.
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                let old = i.get_member(&obj, &k)?;
                step_and_store(i, stack, kind, old, |i, v| i.set_member(&obj, &k, v))?;
            }
            Op::ToPropKey => {
                match stack.last().expect("vm stack underflow") {
                    // Side-effect-free and deterministic to coerce later; numbers stay numeric
                    // so GetElem/SetElem keep their dense fast path.
                    Value::Num(_) | Value::Str(_) => {}
                    _ => {
                        let key = pop!();
                        if matches!(
                            stack.last().expect("vm stack underflow"),
                            Value::Undefined | Value::Null
                        ) {
                            return Err(i.throw(
                                "TypeError",
                                "cannot access property of null or undefined",
                            ));
                        }
                        let k = i.to_property_key(&key)?;
                        stack.push(Value::str(k));
                    }
                }
            }
            Op::Dup2 => {
                let len = stack.len();
                let a = stack[len - 2].clone();
                let b = stack[len - 1].clone();
                stack.push(a);
                stack.push(b);
            }
            Op::GetMethod(n, c) => {
                let obj = pop!();
                let m = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
                stack.push(obj);
                stack.push(m);
            }
            Op::GetMethodElem => {
                let key = pop!();
                let obj = pop!();
                let m = if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_get_elem(o, *n) {
                        Some(v) => v,
                        None => {
                            let k = i.to_property_key(&key)?;
                            i.get_member(&obj, &k)?
                        }
                    }
                } else {
                    if matches!(obj, Value::Undefined | Value::Null) {
                        return Err(
                            i.throw("TypeError", "cannot read property of null or undefined")
                        );
                    }
                    let k = i.to_property_key(&key)?;
                    i.get_member(&obj, &k)?
                };
                stack.push(obj);
                stack.push(m);
            }
            Op::Add => bin_num(i, &mut *stack, "+", |a, b| a + b)?,
            Op::Sub => bin_num(i, &mut *stack, "-", |a, b| a - b)?,
            Op::Mul => bin_num(i, &mut *stack, "*", |a, b| a * b)?,
            Op::Div => bin_num(i, &mut *stack, "/", |a, b| a / b)?,
            Op::Mod => bin_num(i, &mut *stack, "%", crate::eval::js_mod)?,
            Op::BitAnd => bin_i32(i, &mut *stack, "&", |a, b| a & b)?,
            Op::BitOr => bin_i32(i, &mut *stack, "|", |a, b| a | b)?,
            Op::BitXor => bin_i32(i, &mut *stack, "^", |a, b| a ^ b)?,
            Op::Shl => bin_i32(i, &mut *stack, "<<", |a, b| a.wrapping_shl(b as u32 & 31))?,
            Op::Shr => bin_i32(i, &mut *stack, ">>", |a, b| a >> (b as u32 & 31))?,
            Op::UShr => {
                let b = pop!();
                let a = pop!();
                if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
                    let r = (crate::eval::to_int32(*x) as u32)
                        >> (crate::eval::to_int32(*y) as u32 & 31);
                    stack.push(Value::Num(r as f64));
                } else {
                    let v = i.binary(">>>", a, b)?;
                    stack.push(v);
                }
            }
            Op::Lt => bin_cmp(i, &mut *stack, "<", |a, b| a < b)?,
            Op::Gt => bin_cmp(i, &mut *stack, ">", |a, b| a > b)?,
            Op::Le => bin_cmp(i, &mut *stack, "<=", |a, b| a <= b)?,
            Op::Ge => bin_cmp(i, &mut *stack, ">=", |a, b| a >= b)?,
            Op::EqEq => bin_cmp(i, &mut *stack, "==", |a, b| a == b)?,
            Op::NotEq => bin_cmp(i, &mut *stack, "!=", |a, b| a != b)?,
            Op::StrictEq => bin_cmp(i, &mut *stack, "===", |a, b| a == b)?,
            Op::StrictNotEq => bin_cmp(i, &mut *stack, "!==", |a, b| a != b)?,
            Op::GenBin(n) => {
                let b = pop!();
                let a = pop!();
                let v = i.binary(&chunk.names[n as usize], a, b)?;
                stack.push(v);
            }
            Op::Neg => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(-n)),
                    other => {
                        let v = i.eval_unary_vm("-", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Plus => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(n)),
                    other => {
                        let v = i.eval_unary_vm("+", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Not => {
                let a = pop!();
                stack.push(Value::Bool(!i.to_boolean(&a)));
            }
            Op::BitNot => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(!crate::eval::to_int32(n) as f64)),
                    other => {
                        let v = i.eval_unary_vm("~", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Typeof => {
                let a = pop!();
                let v = i.eval_unary_vm("typeof", a)?;
                stack.push(v);
            }
            Op::Void => {
                pop!();
                stack.push(Value::Undefined);
            }
            Op::Jump(t) => *pc = t as usize,
            Op::JumpIfFalse(t) => {
                let a = pop!();
                if !i.to_boolean(&a) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfFalsePeek(t) => {
                if !i.to_boolean(stack.last().expect("vm stack underflow")) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfTruePeek(t) => {
                if i.to_boolean(stack.last().expect("vm stack underflow")) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotNullishPeek(t) => {
                if !matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                ) {
                    *pc = t as usize;
                }
            }
            // Calls pass the argument window as a slice of the operand stack — no per-call `Vec`.
            // The callee/receiver slots below the window are cloned out first, then the whole
            // region is truncated away after the call. On a throw the stack is left long, which is
            // fine: the handler unwind (or function exit) truncates it.
            Op::Call(argc) => {
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = i.call(callee, Value::Undefined, &stack[at..])?;
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::LoadNameForCall(n, c) => {
                // A depth-0 cache hit/fill can't have come through a `with` object: `this` is
                // undefined. Only the full walk can produce a with-object receiver.
                if let Some(v) = chunk.name_ic_hit(env, c).or_else(|| chunk.name_ic_fill(env, n, c))
                {
                    stack.push(Value::Undefined);
                    stack.push(v);
                } else {
                    let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                    stack.push(with_this.unwrap_or(Value::Undefined));
                    stack.push(callee);
                }
            }
            Op::CallWithThis(argc) => {
                let at = stack.len() - argc as usize;
                let m = stack[at - 1].clone();
                let this = stack[at - 2].clone();
                let v = i.call(m, this, &stack[at..])?;
                stack.truncate(at - 2);
                stack.push(v);
            }
            Op::New(argc) => {
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = i.construct(callee, &stack[at..])?;
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::MakeArray(n) => {
                let at = stack.len() - n as usize;
                let items: Vec<Value> = stack.split_off(at);
                stack.push(i.make_array(items));
            }
            Op::MakeObject(start, count) => {
                let at = stack.len() - count as usize;
                let values: Vec<Value> = stack.split_off(at);
                let v = i.make_plain_object_vm(
                    &chunk.names[start as usize..start as usize + count as usize],
                    values,
                );
                stack.push(v);
            }
            Op::Throw => {
                let v = pop!();
                return Err(Abrupt::Throw(v));
            }
            Op::Return => return Ok(VmStep::Done(pop!())),
            Op::ReturnUndef => return Ok(VmStep::Done(Value::Undefined)),
            Op::Await => return Ok(VmStep::Await(pop!())),
            Op::PushHandler(catch_pc) => handlers.push(Handler {
                catch_pc: catch_pc as usize,
                stack_depth: stack.len(),
            }),
            Op::PopHandler => {
                handlers.pop();
            }
        }
    }
}

/// An async function body running on the bytecode VM, suspendable at each `await` without an OS
/// thread. It presents the same `resume(&mut Interp, Resume) -> Suspend` shape as the thread-backed
/// coroutine, so the promise driver (`Interp::drive_async`) treats both uniformly — the only cost
/// per await is now a couple of `Vec` swaps instead of a thread handoff.
pub struct VmCoro {
    chunk: Rc<Chunk>,
    env: Env,
    this_val: Value,
    slots: Vec<Value>,
    stack: Vec<Value>,
    pc: usize,
    /// The `try` handler stack, saved across suspensions so a rejected `await` inside a `try` still
    /// lands in its `catch`.
    handlers: Vec<Handler>,
    pub done: bool,
    pub started: bool,
}

impl VmCoro {
    /// Build an async coroutine for `chunk` with params seeded from `args`, parked before its first
    /// step (run on the first `resume`).
    pub fn new(i: &Interp, chunk: Rc<Chunk>, env: Env, this_val: Value, args: &[Value]) -> VmCoro {
        let env = chunk.make_run_env(i, &env, &this_val, args);
        let mut slots = vec![Value::Undefined; chunk.n_slots];
        for (k, a) in args.iter().take(chunk.n_params).enumerate() {
            slots[k] = a.clone();
        }
        for &s in &chunk.var_force_resets {
            slots[s as usize] = Value::Undefined;
        }
        VmCoro {
            chunk,
            env,
            this_val,
            slots,
            stack: Vec::with_capacity(16),
            pc: 0,
            handlers: Vec::new(),
            done: false,
            started: false,
        }
    }

    /// Drive one step: run to the next `await` (`Suspend::Await`), to completion (`Done`), or to an
    /// uncaught throw (`Throw`). A `Resume::Throw` (rejected await) is injected at the await point so
    /// an enclosing `try`/`catch` in the body can catch it; only an uncaught one rejects the function.
    pub fn resume(
        &mut self,
        i: &mut Interp,
        signal: crate::coroutine::Resume,
    ) -> crate::coroutine::Suspend {
        use crate::coroutine::{Resume, Suspend};
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let pending_throw = match signal {
            Resume::Next(v) => {
                if self.started {
                    self.stack.push(v); // the settled value of the await we parked at
                }
                None
            }
            // A rejected await: re-enter the VM throwing `e` at the suspension point.
            Resume::Throw(e) if self.started => Some(e),
            Resume::Throw(e) => {
                self.done = true;
                return Suspend::Throw(e);
            }
            Resume::Return(v) => {
                self.done = true;
                return Suspend::Done(v);
            }
        };
        self.started = true;
        match drive_vm(
            i,
            &self.chunk,
            &self.env,
            &mut self.slots,
            &mut self.stack,
            &mut self.pc,
            &self.this_val,
            &mut self.handlers,
            pending_throw,
        ) {
            Ok(VmStep::Await(a)) => Suspend::Await(a),
            Ok(VmStep::Done(v)) => {
                self.done = true;
                Suspend::Done(v)
            }
            Err(Abrupt::Throw(e)) => {
                self.done = true;
                Suspend::Throw(e)
            }
            // Return/Break/Continue can't escape a function body; treat defensively as completion.
            Err(_) => {
                self.done = true;
                Suspend::Done(Value::Undefined)
            }
        }
    }
}

/// Shared `++`/`--` tail for property/element updates: ToNumeric the old value, write old±1 back
/// through `set`, and return the value to leave on the stack — old / new / nothing per `kind`.
/// Post variants yield the *coerced* old value, matching the oracle's `eval_update`; a BigInt
/// stays a BigInt.
fn step_value(
    i: &mut Interp,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<Option<Value>, Abrupt> {
    let inc = matches!(
        kind,
        UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard
    );
    Ok(match old {
        Value::BigInt(n) => {
            let one = crate::bigint::JsBigInt::from_u64(1);
            let new = if inc { n.add(&one) } else { n.sub(&one) };
            set(i, Value::BigInt(new.clone()))?;
            match kind {
                UpdKind::PreInc | UpdKind::PreDec => Some(Value::BigInt(new)),
                UpdKind::PostInc | UpdKind::PostDec => Some(Value::BigInt(n)),
                UpdKind::IncDiscard | UpdKind::DecDiscard => None,
            }
        }
        other => {
            let oldn = match other {
                Value::Num(n) => n,
                other => i.to_number(&other)?,
            };
            let new = if inc { oldn + 1.0 } else { oldn - 1.0 };
            set(i, Value::Num(new))?;
            match kind {
                UpdKind::PreInc | UpdKind::PreDec => Some(Value::Num(new)),
                UpdKind::PostInc | UpdKind::PostDec => Some(Value::Num(oldn)),
                UpdKind::IncDiscard | UpdKind::DecDiscard => None,
            }
        }
    })
}

/// [`step_value`] pushing its result onto the VM's operand stack.
fn step_and_store(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<(), Abrupt> {
    if let Some(v) = step_value(i, kind, old, set)? {
        stack.push(v);
    }
    Ok(())
}

#[inline]
fn bin_num(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Num(f(*x, *y)));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn bin_i32(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Num(
            f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64,
        ));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn bin_cmp(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Bool(f(*x, *y)));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// JIT support: Chunk accessors and the runtime helpers the machine-code templates call.
// The generic executor `jit_exec` runs exactly ONE op against a raw operand-stack pointer; the
// templates bake the op index in as an immediate and keep the stack top in a register. Control
// flow never reaches here — jumps, returns and try bookkeeping are real branches in the JIT.
// ---------------------------------------------------------------------------------------------

impl Chunk {
    pub(crate) fn jit_ops(&self) -> &[Op] {
        &self.ops
    }
    /// The stable address of inline-cache site `idx`'s `Cell<IcState>`. The `caches` `Vec` is
    /// fixed once compilation finishes (never reallocated), and the `Chunk` outlives its own JIT
    /// code, so the JIT bakes this address as an immediate to read the live cache from machine
    /// code. `None` if the emitter cannot use it (the address must be reachable — always is here).
    pub(crate) fn jit_cache_ptr(&self, idx: u32) -> usize {
        self.caches.as_ptr() as usize
            + idx as usize * std::mem::size_of::<std::cell::Cell<IcState>>()
    }
    /// The stable address of name-cache site `idx`'s `Cell<NameIc>` (same contract as
    /// [`Chunk::jit_cache_ptr`]).
    pub(crate) fn jit_name_cache_ptr(&self, idx: u32) -> usize {
        self.name_caches.as_ptr() as usize
            + idx as usize * std::mem::size_of::<std::cell::Cell<NameIc>>()
    }
    /// Name-cache hit check (see [`NameIc`] for the validation story): a pointer compare, a
    /// generation compare, and a value clone. `None` = miss (including TDZ — the slow path
    /// throws the proper error).
    #[inline]
    fn name_ic_hit(&self, env: &Env, c: u32) -> Option<Value> {
        let ic = self.name_caches[c as usize].get();
        if ic.env == 0 || ic.env != Rc::as_ptr(env) as usize {
            return None;
        }
        let b = env.borrow();
        if b.vars.generation() != ic.gen {
            return None;
        }
        // The unchanged generation proves the map is structurally untouched since the fill: the
        // pointer is live and the resolution unchanged (see NameIc). The value and TDZ flag are
        // read live — in-place writes flow through.
        let bd = unsafe { &*(ic.binding as *const crate::interpreter::Binding) };
        if bd.initialized {
            Some(bd.value.clone())
        } else {
            None
        }
    }
    /// Depth-0 cache fill: the name resolves directly in `env` as a plain initialized binding —
    /// no `with` object on the scope, no live import redirect. Returns the value on success;
    /// `None` = not depth-0-cacheable (the caller runs the interpreter's full walk, uncached).
    fn name_ic_fill(&self, env: &Env, n: u32, c: u32) -> Option<Value> {
        let b = env.borrow();
        if b.with_obj.is_some() {
            return None;
        }
        let bd = b.vars.get(&*self.names[n as usize])?;
        if !bd.initialized || bd.import_ref.is_some() {
            return None;
        }
        let v = bd.value.clone();
        self.name_caches[c as usize].set(NameIc {
            env: Rc::as_ptr(env) as usize,
            binding: bd as *const _ as usize,
            gen: b.vars.generation(),
            _pad: 0,
        });
        drop(b);
        // Pin the scope allocation so the raw `env` compare stays ABA-safe.
        self.name_pins.borrow_mut()[c as usize] = Some(Rc::downgrade(env));
        Some(v)
    }
    /// Cached free-name read: hit, else depth-0 refill, else the interpreter's full walk
    /// (deeper resolutions, `with`, module imports, TDZ, globals — uncached every time).
    fn load_name_ic(&self, i: &mut Interp, env: &Env, n: u32, c: u32) -> Result<Value, Abrupt> {
        if let Some(v) = self.name_ic_hit(env, c) {
            return Ok(v);
        }
        if let Some(v) = self.name_ic_fill(env, n, c) {
            return Ok(v);
        }
        i.get_var(&self.names[n as usize], env)
    }
    pub(crate) fn jit_frame(&self) -> (usize, usize) {
        (self.n_params, self.n_slots)
    }
    pub(crate) fn jit_var_force_resets(&self) -> &[u16] {
        &self.var_force_resets
    }
    /// Whether const `k` is a trivially-copyable value the JIT may materialize inline.
    pub(crate) fn jit_const_copyable(&self, k: u32) -> bool {
        matches!(
            self.consts[k as usize],
            Value::Undefined | Value::Null | Value::Bool(_) | Value::Num(_)
        )
    }
    /// The f64 bits of a Num const (for the JIT's register-chain emitter).
    pub(crate) fn jit_const_num(&self, k: u32) -> Option<u64> {
        match &self.consts[k as usize] {
            Value::Num(n) => Some(n.to_bits()),
            _ => None,
        }
    }
    /// The first 16 bytes of a copyable const as two words, for inline materialization.
    /// repr(u8) puts each payload at its own alignment: Bool's byte sits in word0 at offset 1,
    /// Num's f64 fills word1 (offset 8).
    pub(crate) fn jit_const_bits(&self, k: u32) -> (u64, u64) {
        match &self.consts[k as usize] {
            Value::Undefined => (0, 0),
            Value::Null => (2, 0),
            Value::Bool(b) => (3 | ((*b as u64) << 8), 0),
            Value::Num(n) => (4, n.to_bits()),
            _ => unreachable!("non-copyable const in jit_const_bits"),
        }
    }
    pub(crate) fn jit_make_run_env(
        &self,
        i: &Interp,
        env: &Env,
        this_val: &Value,
        args: &[Value],
    ) -> Env {
        self.make_run_env(i, env, this_val, args)
    }
    /// (pops, pushes) of the op at `pc`, for the static stack-depth analysis. `None` = an op the
    /// JIT can't account for (which refuses compilation).
    pub(crate) fn jit_stack_effect(&self, pc: usize) -> Option<(usize, usize)> {
        let upd = |k: &UpdKind| match k {
            UpdKind::IncDiscard | UpdKind::DecDiscard => 0,
            _ => 1,
        };
        Some(match &self.ops[pc] {
            Op::Const(_)
            | Op::Undef
            | Op::LoadLocal(_)
            | Op::LoadCap(_)
            | Op::LoadName(..)
            | Op::LoadThis
            | Op::MakeClosure(..) => (0, 1),
            Op::Dup => (1, 2),
            Op::Dup2 => (2, 4),
            Op::Pop
            | Op::StoreLocal(_)
            | Op::StoreCap(_)
            | Op::StoreCapInit(_)
            | Op::StoreName(_) => (1, 0),
            Op::UpdateLocal(_, k) | Op::UpdateCap(_, k) => (0, upd(k)),
            Op::UpdateProp(_, _, k) => (1, upd(k)),
            Op::UpdateElem(k) => (2, upd(k)),
            Op::Tdz(_) => (0, 0),
            Op::GetProp(..) => (1, 1),
            Op::SetProp(..) => (2, 1),
            Op::SetPropDrop(..) => (2, 0),
            Op::GetElem => (2, 1),
            Op::SetElem => (3, 1),
            Op::SetElemDrop => (3, 0),
            Op::GetElemLocal(_) => (1, 1),
            Op::SetElemLocal(_) => (2, 1),
            Op::SetElemLocalDrop(_) => (2, 0),
            Op::ToPropKey => (2, 2),
            Op::GetMethod(..) => (1, 2),
            Op::GetMethodElem => (2, 2),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::EqEq
            | Op::NotEq
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::GenBin(_) => (2, 1),
            Op::Neg | Op::Plus | Op::Not | Op::BitNot | Op::Typeof | Op::Void => (1, 1),
            Op::Jump(_) => (0, 0),
            Op::JumpIfFalse(_) => (1, 0),
            Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::JumpIfNotNullishPeek(_) => (1, 1),
            Op::Call(argc) => (*argc as usize + 1, 1),
            Op::LoadNameForCall(..) => (0, 2),
            Op::CallWithThis(argc) => (*argc as usize + 2, 1),
            Op::New(argc) => (*argc as usize + 1, 1),
            Op::MakeArray(n) => (*n as usize, 1),
            Op::MakeObject(_, count) => (*count as usize, 1),
            Op::Throw | Op::Return => (1, 0),
            Op::ReturnUndef => (0, 0),
            Op::Await => (1, 1),
            Op::PushHandler(_) | Op::PopHandler => (0, 0),
        })
    }
}

/// Execute the single (non-control-flow) op at `pc` against the raw operand stack `sp`. Returns
/// the updated stack top (reflecting any operands consumed *even on a throw* — the unwinder's
/// cleanup must never re-drop moved-out slots) plus a flag: 1 = threw (stored in `ctx.error`).
///
/// # Safety
/// Called from JIT code with `ctx` pointing at the live `JitCtx` for this activation and `sp`
/// inside its stack buffer, whose capacity covers the chunk's statically-computed maximum depth.
pub(crate) unsafe extern "C" fn jit_exec(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    match jit_exec_inner(ctx, pc, &mut sp) {
        Ok(()) => crate::jit::SpFlag { sp, flag: 0 },
        Err(ab) => {
            ctx.error = Some(ab);
            crate::jit::SpFlag { sp, flag: 1 }
        }
    }
}

unsafe fn jit_exec_inner(
    ctx: &mut crate::jit::JitCtx,
    pc: u32,
    sp: &mut *mut Value,
) -> Result<(), Abrupt> {
    let i = &mut *ctx.interp;
    let chunk = &*ctx.chunk;
    let env = &ctx.env;
    let slots = std::slice::from_raw_parts_mut(ctx.slots, ctx.n_slots);
    macro_rules! pop {
        () => {{
            *sp = sp.sub(1);
            sp.read()
        }};
    }
    macro_rules! push {
        ($v:expr) => {{
            sp.write($v);
            *sp = sp.add(1);
        }};
    }
    match chunk.ops[pc as usize] {
        Op::Const(k) => push!(chunk.consts[k as usize].clone()),
        Op::Undef => push!(Value::Undefined),
        Op::Dup => {
            let t = (*sp.sub(1)).clone();
            push!(t);
        }
        Op::Pop => {
            pop!();
        }
        Op::Dup2 => {
            let a = (*sp.sub(2)).clone();
            let b = (*sp.sub(1)).clone();
            push!(a);
            push!(b);
        }
        Op::LoadLocal(s) => {
            let v = slots[s as usize].clone();
            if matches!(v, Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[s as usize]
                    ),
                ));
            }
            push!(v);
        }
        Op::StoreLocal(s) => slots[s as usize] = pop!(),
        Op::UpdateLocal(s, kind) => {
            let idx = s as usize;
            if matches!(slots[idx], Value::Empty) {
                return Err(i.throw(
                    "ReferenceError",
                    format!(
                        "cannot access '{}' before initialization",
                        chunk.slot_names[idx]
                    ),
                ));
            }
            let old = slots[idx].clone();
            if let Some(v) = step_value(i, kind, old, |_, v| {
                slots[idx] = v;
                Ok(())
            })? {
                push!(v);
            }
        }
        Op::Tdz(s) => slots[s as usize] = Value::Empty,
        Op::LoadCap(n) => {
            let name = &chunk.names[n as usize];
            let b = env.borrow();
            let bd = b.vars.get(&**name).expect("captured binding missing");
            if !bd.initialized {
                let msg = format!("cannot access '{name}' before initialization");
                drop(b);
                return Err(i.throw("ReferenceError", msg));
            }
            let v = bd.value.clone();
            drop(b);
            push!(v);
        }
        Op::StoreCap(n) => {
            let name = &chunk.names[n as usize];
            let v = pop!();
            let mut b = env.borrow_mut();
            let bd = b.vars.get_mut(name).expect("captured binding missing");
            if !bd.initialized {
                let msg = format!("cannot access '{name}' before initialization");
                drop(b);
                return Err(i.throw("ReferenceError", msg));
            }
            bd.value = v;
        }
        Op::StoreCapInit(n) => {
            let name = &chunk.names[n as usize];
            let v = pop!();
            let mut b = env.borrow_mut();
            let bd = b.vars.get_mut(name).expect("captured binding missing");
            bd.value = v;
            bd.initialized = true;
        }
        Op::UpdateCap(n, kind) => {
            let name = &chunk.names[n as usize];
            let old = {
                let b = env.borrow();
                let bd = b.vars.get(&**name).expect("captured binding missing");
                if !bd.initialized {
                    let msg = format!("cannot access '{name}' before initialization");
                    drop(b);
                    return Err(i.throw("ReferenceError", msg));
                }
                bd.value.clone()
            };
            if let Some(v) = step_value(i, kind, old, |_, v| {
                if let Some(bd) = env.borrow_mut().vars.get_mut(name) {
                    bd.value = v;
                }
                Ok(())
            })? {
                push!(v);
            }
        }
        Op::MakeClosure(fidx, name_n) => {
            let v = i.make_function(chunk.funcs[fidx as usize].clone(), env.clone());
            if name_n != u32::MAX {
                i.set_fn_name(&v, &chunk.names[name_n as usize]);
            }
            push!(v);
        }
        Op::LoadName(n, c) => {
            let v = chunk.load_name_ic(i, env, n, c)?;
            push!(v);
        }
        Op::StoreName(n) => {
            let v = pop!();
            i.assign_free_name(&chunk.names[n as usize], v, env)?;
        }
        Op::LoadThis => push!(ctx.this_val.clone()),
        Op::GetProp(n, c) => {
            let obj = pop!();
            let v = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
            push!(v);
        }
        Op::SetProp(n, c) => {
            let v = pop!();
            let obj = pop!();
            i.set_prop_ic(&obj, &chunk.names[n as usize], v.clone(), &chunk.caches[c as usize])?;
            push!(v);
        }
        Op::SetPropDrop(n, c) => {
            let v = pop!();
            let obj = pop!();
            i.set_prop_ic(&obj, &chunk.names[n as usize], v, &chunk.caches[c as usize])?;
        }
        Op::GetElem => {
            let key = pop!();
            let obj = pop!();
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                if let Some(v) = i.fast_get_elem(o, *n) {
                    push!(v);
                    return Ok(());
                }
            }
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let v = i.get_member(&obj, &k)?;
            push!(v);
        }
        Op::SetElem => {
            let v = pop!();
            let key = pop!();
            let obj = pop!();
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                let ret = v.clone();
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => {
                        push!(ret);
                        return Ok(());
                    }
                    Err(back) => {
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        push!(ret);
                        return Ok(());
                    }
                }
            }
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v.clone())?;
            push!(v);
        }
        Op::SetElemDrop => {
            let v = pop!();
            let key = pop!();
            let obj = pop!();
            if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => return Ok(()),
                    Err(back) => {
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        return Ok(());
                    }
                }
            }
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v)?;
        }
        Op::GetElemLocal(s) => {
            let key = pop!();
            if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                if let Some(v) = i.fast_get_elem(o, *n) {
                    push!(v);
                    return Ok(());
                }
            }
            let obj = slots[s as usize].clone();
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let v = i.get_member(&obj, &k)?;
            push!(v);
        }
        Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
            let keep = matches!(chunk.ops[pc as usize], Op::SetElemLocal(_));
            let v = pop!();
            let key = pop!();
            if keep {
                push!(v.clone());
            }
            if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                match i.fast_set_elem(o, *n, v) {
                    Ok(()) => return Ok(()),
                    Err(back) => {
                        let obj = slots[s as usize].clone();
                        let k = i.to_property_key(&key)?;
                        i.set_member(&obj, &k, back)?;
                        return Ok(());
                    }
                }
            }
            let obj = slots[s as usize].clone();
            let k = i.to_property_key(&key)?;
            i.set_member(&obj, &k, v)?;
        }
        Op::UpdateProp(n, c, kind) => {
            let obj = pop!();
            let name = &chunk.names[n as usize];
            let cache = &chunk.caches[c as usize];
            let old = i.get_prop_ic(&obj, name, cache)?;
            if let Some(v) = step_value(i, kind, old, |i, v| i.set_prop_ic(&obj, name, v, cache))? {
                push!(v);
            }
        }
        Op::UpdateElem(kind) => {
            let key = pop!();
            let obj = pop!();
            if let (Value::Obj(o), Value::Num(nk)) = (&obj, &key) {
                if let Some(Value::Num(old)) = i.fast_get_elem(o, *nk) {
                    let new = match kind {
                        UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                        UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                    };
                    if i.fast_set_elem(o, *nk, Value::Num(new)).is_ok() {
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => push!(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => push!(Value::Num(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                        return Ok(());
                    }
                }
            }
            if matches!(obj, Value::Undefined | Value::Null) {
                return Err(i.throw("TypeError", "cannot read property of null or undefined"));
            }
            let k = i.to_property_key(&key)?;
            let old = i.get_member(&obj, &k)?;
            if let Some(v) = step_value(i, kind, old, |i, v| i.set_member(&obj, &k, v))? {
                push!(v);
            }
        }
        Op::ToPropKey => {
            match &*sp.sub(1) {
                Value::Num(_) | Value::Str(_) => {}
                _ => {
                    let key = pop!();
                    if matches!(&*sp.sub(1), Value::Undefined | Value::Null) {
                        return Err(
                            i.throw("TypeError", "cannot access property of null or undefined")
                        );
                    }
                    let k = i.to_property_key(&key)?;
                    push!(Value::str(k));
                }
            }
        }
        Op::GetMethod(n, c) => {
            let obj = pop!();
            let m = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
            push!(obj);
            push!(m);
        }
        Op::GetMethodElem => {
            let key = pop!();
            let obj = pop!();
            let m = if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                match i.fast_get_elem(o, *n) {
                    Some(v) => v,
                    None => {
                        let k = i.to_property_key(&key)?;
                        i.get_member(&obj, &k)?
                    }
                }
            } else {
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", "cannot read property of null or undefined"));
                }
                let k = i.to_property_key(&key)?;
                i.get_member(&obj, &k)?
            };
            push!(obj);
            push!(m);
        }
        Op::Add => jit_bin_num(i, sp, "+", |a, b| a + b)?,
        Op::Sub => jit_bin_num(i, sp, "-", |a, b| a - b)?,
        Op::Mul => jit_bin_num(i, sp, "*", |a, b| a * b)?,
        Op::Div => jit_bin_num(i, sp, "/", |a, b| a / b)?,
        Op::Mod => jit_bin_num(i, sp, "%", crate::eval::js_mod)?,
        Op::BitAnd => jit_bin_i32(i, sp, "&", |a, b| a & b)?,
        Op::BitOr => jit_bin_i32(i, sp, "|", |a, b| a | b)?,
        Op::BitXor => jit_bin_i32(i, sp, "^", |a, b| a ^ b)?,
        Op::Shl => jit_bin_i32(i, sp, "<<", |a, b| a.wrapping_shl(b as u32 & 31))?,
        Op::Shr => jit_bin_i32(i, sp, ">>", |a, b| a >> (b as u32 & 31))?,
        Op::UShr => {
            let b = pop!();
            let a = pop!();
            if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
                let r =
                    (crate::eval::to_int32(*x) as u32) >> (crate::eval::to_int32(*y) as u32 & 31);
                push!(Value::Num(r as f64));
            } else {
                let v = i.binary(">>>", a, b)?;
                push!(v);
            }
        }
        Op::Lt => jit_bin_cmp(i, sp, "<", |a, b| a < b)?,
        Op::Gt => jit_bin_cmp(i, sp, ">", |a, b| a > b)?,
        Op::Le => jit_bin_cmp(i, sp, "<=", |a, b| a <= b)?,
        Op::Ge => jit_bin_cmp(i, sp, ">=", |a, b| a >= b)?,
        Op::EqEq => jit_bin_cmp(i, sp, "==", |a, b| a == b)?,
        Op::NotEq => jit_bin_cmp(i, sp, "!=", |a, b| a != b)?,
        Op::StrictEq => jit_bin_cmp(i, sp, "===", |a, b| a == b)?,
        Op::StrictNotEq => jit_bin_cmp(i, sp, "!==", |a, b| a != b)?,
        Op::GenBin(n) => {
            let b = pop!();
            let a = pop!();
            let v = i.binary(&chunk.names[n as usize], a, b)?;
            push!(v);
        }
        Op::Neg => {
            let a = pop!();
            match a {
                Value::Num(n) => push!(Value::Num(-n)),
                other => {
                    let v = i.eval_unary_vm("-", other)?;
                    push!(v);
                }
            }
        }
        Op::Plus => {
            let a = pop!();
            match a {
                Value::Num(n) => push!(Value::Num(n)),
                other => {
                    let v = i.eval_unary_vm("+", other)?;
                    push!(v);
                }
            }
        }
        Op::Not => {
            let a = pop!();
            let b = !i.to_boolean(&a);
            push!(Value::Bool(b));
        }
        Op::BitNot => {
            let a = pop!();
            match a {
                Value::Num(n) => push!(Value::Num(!crate::eval::to_int32(n) as f64)),
                other => {
                    let v = i.eval_unary_vm("~", other)?;
                    push!(v);
                }
            }
        }
        Op::Typeof => {
            let a = pop!();
            let v = i.eval_unary_vm("typeof", a)?;
            push!(v);
        }
        Op::Void => {
            pop!();
            push!(Value::Undefined);
        }
        Op::Call(argc) => {
            let argc = argc as usize;
            let args = std::slice::from_raw_parts(sp.sub(argc), argc);
            let callee = (*sp.sub(argc + 1)).clone();
            let v = i.call(callee, Value::Undefined, args)?;
            *sp = jit_consume(*sp, argc + 1);
            push!(v);
        }
        Op::LoadNameForCall(n, c) => {
            // A depth-0 cache hit/fill can't have come through a `with` object (see the VM arm).
            if let Some(v) = chunk.name_ic_hit(env, c).or_else(|| chunk.name_ic_fill(env, n, c)) {
                push!(Value::Undefined);
                push!(v);
            } else {
                let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                push!(with_this.unwrap_or(Value::Undefined));
                push!(callee);
            }
        }
        Op::CallWithThis(argc) => {
            let argc = argc as usize;
            let args = std::slice::from_raw_parts(sp.sub(argc), argc);
            let m = (*sp.sub(argc + 1)).clone();
            let this = (*sp.sub(argc + 2)).clone();
            let v = i.call(m, this, args)?;
            *sp = jit_consume(*sp, argc + 2);
            push!(v);
        }
        Op::New(argc) => {
            let argc = argc as usize;
            let args = std::slice::from_raw_parts(sp.sub(argc), argc);
            let callee = (*sp.sub(argc + 1)).clone();
            let v = i.construct(callee, args)?;
            *sp = jit_consume(*sp, argc + 1);
            push!(v);
        }
        Op::MakeArray(n) => {
            let n = n as usize;
            let mut items = Vec::with_capacity(n);
            let base = sp.sub(n);
            for k in 0..n {
                items.push(base.add(k).read());
            }
            *sp = base;
            push!(i.make_array(items));
        }
        Op::MakeObject(start, count) => {
            let count = count as usize;
            let mut values = Vec::with_capacity(count);
            let base = sp.sub(count);
            for k in 0..count {
                values.push(base.add(k).read());
            }
            *sp = base;
            let v = i.make_plain_object_vm(
                &chunk.names[start as usize..start as usize + count],
                values,
            );
            push!(v);
        }
        Op::Throw => {
            let v = pop!();
            return Err(Abrupt::Throw(v));
        }
        Op::Jump(_)
        | Op::JumpIfFalse(_)
        | Op::JumpIfFalsePeek(_)
        | Op::JumpIfTruePeek(_)
        | Op::JumpIfNotNullishPeek(_)
        | Op::Return
        | Op::ReturnUndef
        | Op::Await
        | Op::PushHandler(_)
        | Op::PopHandler => unreachable!("control-flow op reached jit_exec"),
    }
    Ok(())
}

/// Drop `n` consumed operands below `sp` (post-call cleanup) and return the new top.
unsafe fn jit_consume(sp: *mut Value, n: usize) -> *mut Value {
    let base = sp.sub(n);
    for k in 0..n {
        std::ptr::drop_in_place(base.add(k));
    }
    base
}

unsafe fn jit_bin_num(
    i: &mut Interp,
    sp: &mut *mut Value,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Num(f(*x, *y))
    } else {
        i.binary(op, a, b)?
    };
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

unsafe fn jit_bin_i32(
    i: &mut Interp,
    sp: &mut *mut Value,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Num(f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64)
    } else {
        i.binary(op, a, b)?
    };
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

unsafe fn jit_bin_cmp(
    i: &mut Interp,
    sp: &mut *mut Value,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    *sp = sp.sub(1);
    let b = sp.read();
    *sp = sp.sub(1);
    let a = sp.read();
    let v = if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        Value::Bool(f(*x, *y))
    } else {
        i.binary(op, a, b)?
    };
    sp.write(v);
    *sp = sp.add(1);
    Ok(())
}

/// Conditional-branch helper: evaluates the branch predicate per `mode` (see `jit::COND_*`),
/// returning the new sp and the flag. `to_boolean` cannot throw, so sp is never null here.
pub(crate) unsafe extern "C" fn jit_cond(
    ctx: *mut crate::jit::JitCtx,
    mode: u32,
    mut sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    let i = &mut *ctx.interp;
    let flag = match mode {
        crate::jit::COND_POP_TRUTHY => {
            sp = sp.sub(1);
            let v = sp.read();
            i.to_boolean(&v) as u64
        }
        crate::jit::COND_PEEK_TRUTHY => i.to_boolean(&*sp.sub(1)) as u64,
        _ => !matches!(&*sp.sub(1), Value::Undefined | Value::Null) as u64,
    };
    crate::jit::SpFlag { sp, flag }
}

/// Return helper: mode 1 pops the return value into `ctx.ret`; mode 0 returns undefined.
pub(crate) unsafe extern "C" fn jit_return(
    ctx: *mut crate::jit::JitCtx,
    mode: u32,
    mut sp: *mut Value,
) -> *mut Value {
    let ctx = &mut *ctx;
    ctx.ret = if mode == 1 {
        sp = sp.sub(1);
        sp.read()
    } else {
        Value::Undefined
    };
    sp
}

pub(crate) unsafe extern "C" fn jit_push_handler(
    ctx: *mut crate::jit::JitCtx,
    catch_pc: u32,
    sp: *mut Value,
) -> *mut Value {
    let ctx = &mut *ctx;
    let depth = sp.offset_from(ctx.stack_base) as usize;
    ctx.handlers.push((catch_pc, depth));
    sp
}

pub(crate) unsafe extern "C" fn jit_pop_handler(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> *mut Value {
    (*ctx).handlers.pop();
    sp
}

/// Throw routing: land on the innermost `try` handler (returning its code address and the
/// unwound sp with the exception pushed), or (0, sp) to leave the function throwing.
pub(crate) unsafe extern "C" fn jit_unwind(
    ctx: *mut crate::jit::JitCtx,
    _imm: u32,
    sp: *mut Value,
) -> crate::jit::SpFlag {
    let ctx = &mut *ctx;
    // Only thrown completions are catchable; anything else propagates out.
    if !matches!(ctx.error, Some(Abrupt::Throw(_))) {
        return crate::jit::SpFlag { sp: std::ptr::null_mut(), flag: sp as u64 };
    }
    match ctx.handlers.pop() {
        None => crate::jit::SpFlag { sp: std::ptr::null_mut(), flag: sp as u64 },
        Some((catch_pc, depth)) => {
            let target = ctx.stack_base.add(depth);
            // Drop operands above the handler's depth.
            let mut p = target;
            while p < sp {
                std::ptr::drop_in_place(p);
                p = p.add(1);
            }
            let Some(Abrupt::Throw(exc)) = ctx.error.take() else {
                unreachable!()
            };
            target.write(exc);
            let addr = ctx.code_base as usize
                + *ctx.pc_offsets.add(catch_pc as usize) as usize;
            crate::jit::SpFlag {
                sp: addr as *mut Value,
                flag: target.add(1) as u64,
            }
        }
    }
}
