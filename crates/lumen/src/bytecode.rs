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

/// Execution tier. `Interp` must not touch any codegen path at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Interp,
    Bytecode,
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
    /// `++`/`--` on a local slot, done in place (no LoadLocal/Plus/Add dance). Applies ToNumeric
    /// so a BigInt slot stays a BigInt — the `Plus`-based lowering this replaces was ToNumber and
    /// wrongly threw on BigInt. The `UpdKind` says increment vs decrement and which value (old,
    /// new, or none in statement position) to leave on the stack.
    UpdateLocal(u16, UpdKind),
    /// Put the slot into its temporal dead zone (block entry for `let`/`const`).
    Tdz(u16),
    LoadName(u32),
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
    /// `obj.name` as a call target: pops obj, pushes obj then the method (get runs before args).
    GetMethod(u32),
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
    LoadNameForCall(u32),
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
    /// One inline-cache slot per property-access op (`GetProp`/`SetProp`/`SetPropDrop`), holding the
    /// `entries` slot last seen for that site (`u32::MAX` = empty). The `Chunk` is shared across
    /// calls via `Rc`, so these persist. `Cell` is fine: the VM runs one thread at a time (coroutine
    /// ping-pong), like the rest of the engine's shared-`Rc` state.
    caches: Vec<std::cell::Cell<u32>>,
}

impl Chunk {
    /// Whether the body reads `this` (so the driver seeds it from the activation scope).
    pub fn uses_this(&self) -> bool {
        self.uses_this
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({} ops, {} slots)", self.ops.len(), self.n_slots)
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
    let mut c = Compiler::default();
    // Parameters: plain identifiers only, one slot each (a sloppy duplicate name resolves to the
    // later parameter, matching the env behavior where the later insert wins).
    for p in &func.params {
        if p.default.is_some() || p.rest {
            return None;
        }
        let Pattern::Ident(name) = &p.pattern else {
            return None;
        };
        let slot = c.fresh_slot(name);
        c.scope_bind(name, slot, false);
    }
    c.n_params = func.params.len();
    // Function-scoped `var`s (and the oracle's for-head reset quirk) from the shared hoist plan.
    for op in crate::interpreter::collect_hoist_ops(&func.body, func.is_strict, &[]) {
        match op {
            HoistOp::Var(name) => {
                if c.lookup(&name).is_none() {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                }
            }
            HoistOp::VarForce(name) => {
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
            // Inner function declarations (and Annex B promotions) mean closures — bail.
            HoistOp::Fn(..) | HoistOp::AnnexB(..) => return None,
        }
    }
    // Body-level lexicals get TDZ slots.
    c.declare_block_lexicals(&func.body).ok()?;
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
        caches: c.caches,
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
    caches: Vec<std::cell::Cell<u32>>,
}

#[derive(Default)]
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// Labels naming this loop (usually zero or one; `a: b: for(…)` stacks several). A labelled
    /// `break`/`continue` searches the loop stack for the ctx carrying its target label.
    labels: Vec<String>,
}

/// Compilation bail: the construct is outside the v0 subset.
struct Bail;
type CResult = Result<(), Bail>;

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }
    /// Reserve a fresh inline-cache slot (starts empty) for a property-access op.
    fn new_cache(&mut self) -> u32 {
        self.caches.push(std::cell::Cell::new(u32::MAX));
        (self.caches.len() - 1) as u32
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
            Stmt::VarDecl { kind, decls } => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                for (pat, init) in decls {
                    let Pattern::Ident(name) = pat else {
                        return Err(Bail);
                    };
                    let (slot, _) = self.lookup(name).ok_or(Bail)?;
                    match init {
                        Some(e) => self.expr(e)?,
                        // `var x;` leaves an existing binding alone; `let x;` initializes.
                        None => {
                            if matches!(kind, DeclKind::Var) {
                                continue;
                            }
                            self.emit(Op::Undef);
                        }
                    }
                    self.emit(Op::StoreLocal(slot));
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
                self.loops.last_mut().ok_or(Bail)?.continues.push(j);
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
                self.labeled_loop_mut(name).ok_or(Bail)?.continues.push(j);
                Ok(())
            }
            // A label naming a loop attaches to that loop's context; stacked labels (`a: b: for`)
            // accumulate through the recursion. A label on any other statement bails.
            Stmt::Labeled { label, body } => match &**body {
                Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } | Stmt::Labeled { .. } => {
                    self.pending_labels.push(label.clone());
                    self.stmt(body)
                }
                _ => Err(Bail),
            },
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
                if let Expr::Ident(name) = &**arg {
                    if let Some((slot, is_const)) = self.lookup(name) {
                        if !is_const {
                            let kind = match *op {
                                "++" => UpdKind::IncDiscard,
                                "--" => UpdKind::DecDiscard,
                                _ => return Err(Bail),
                            };
                            self.emit(Op::UpdateLocal(slot, kind));
                            return Ok(());
                        }
                    }
                }
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
            Expr::Ident(name) => match self.lookup(name) {
                Some((slot, is_const)) => {
                    if is_const {
                        return Ok(false);
                    }
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreLocal(slot));
                    Ok(true)
                }
                None => {
                    if op != "=" {
                        return Ok(false);
                    }
                    // StoreName already consumes the value without re-pushing it.
                    self.expr(value)?;
                    let i = self.name_idx(name);
                    self.emit(Op::StoreName(i));
                    Ok(true)
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') && op == "=" => {
                self.expr(obj)?;
                self.expr(value)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::SetPropDrop(i, c));
                Ok(true)
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) && op == "=" => {
                self.expr(obj)?;
                self.expr(index)?;
                self.expr(value)?;
                self.emit(Op::SetElemDrop);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn expr(&mut self, e: &Expr) -> CResult {
        match e {
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
            Expr::Ident(name) => {
                match self.lookup(name) {
                    Some((slot, _)) => self.emit(Op::LoadLocal(slot)),
                    None => {
                        let i = self.name_idx(name);
                        self.emit(Op::LoadName(i))
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
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::GetElem);
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
                        if matches!(&**arg, Expr::Ident(n) if self.lookup(n).is_none()) {
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
                let Expr::Ident(name) = &**arg else {
                    return Err(Bail);
                };
                let Some((slot, is_const)) = self.lookup(name) else {
                    return Err(Bail);
                };
                if is_const {
                    return Err(Bail);
                }
                let kind = match (*op, *prefix) {
                    ("++", true) => UpdKind::PreInc,
                    ("--", true) => UpdKind::PreDec,
                    ("++", false) => UpdKind::PostInc,
                    ("--", false) => UpdKind::PostDec,
                    _ => return Err(Bail),
                };
                self.emit(Op::UpdateLocal(slot, kind));
                Ok(())
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
                        self.emit(Op::GetMethod(i));
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
                    Expr::Ident(name) if self.lookup(name).is_none() => {
                        // Free-name callee: resolved before the arguments (spec order), and a
                        // `with (obj) f()` hit supplies obj as `this`.
                        let i = self.name_idx(name);
                        self.emit(Op::LoadNameForCall(i));
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
                    keys.push(k);
                    self.expr(value)?;
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

    fn assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if matches!(op, "&&=" | "||=" | "??=") {
            return Err(Bail);
        }
        match target {
            Expr::Ident(name) => match self.lookup(name) {
                Some((slot, is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    Ok(())
                }
                None => {
                    if op != "=" {
                        return Err(Bail);
                    }
                    self.expr(value)?;
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
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') && op == "=" => {
                self.expr(obj)?;
                self.expr(value)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::SetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) && op == "=" => {
                self.expr(obj)?;
                self.expr(index)?;
                self.expr(value)?;
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

/// Execute a compiled function body. `env` is the activation environment (used only as the root
/// for free-name resolution); parameters are seeded straight into slots. Synchronous bodies only —
/// an async body runs through [`VmCoro`], which drives [`run_vm`] and can suspend it.
pub fn run(i: &mut Interp, chunk: &Chunk, env: &Env, args: &[Value]) -> Result<Value, Abrupt> {
    let mut slots: Vec<Value> = vec![Value::Undefined; chunk.n_slots];
    for (k, a) in args.iter().take(chunk.n_params).enumerate() {
        slots[k] = a.clone();
    }
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    let this_val = if chunk.uses_this {
        i.get_var("this", env)?
    } else {
        Value::Undefined
    };
    let mut stack: Vec<Value> = Vec::with_capacity(16);
    let mut pc = 0usize;
    let mut handlers: Vec<Handler> = Vec::new();
    match drive_vm(
        i, chunk, env, &mut slots, &mut stack, &mut pc, &this_val, &mut handlers, None,
    )? {
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
    slots: &mut Vec<Value>,
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
    slots: &mut Vec<Value>,
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
            Op::LoadName(n) => {
                let v = i.get_var(&chunk.names[n as usize], env)?;
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
            Op::GetMethod(n) => {
                let obj = pop!();
                let m = i.get_member(&obj, &chunk.names[n as usize])?;
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
            Op::Call(argc) => {
                let at = stack.len() - argc as usize;
                let args: Vec<Value> = stack.split_off(at);
                let callee = pop!();
                let v = i.call(callee, Value::Undefined, &args)?;
                stack.push(v);
            }
            Op::LoadNameForCall(n) => {
                let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                stack.push(with_this.unwrap_or(Value::Undefined));
                stack.push(callee);
            }
            Op::CallWithThis(argc) => {
                let at = stack.len() - argc as usize;
                let args: Vec<Value> = stack.split_off(at);
                let m = pop!();
                let this = pop!();
                let v = i.call(m, this, &args)?;
                stack.push(v);
            }
            Op::New(argc) => {
                let at = stack.len() - argc as usize;
                let args: Vec<Value> = stack.split_off(at);
                let callee = pop!();
                let v = i.construct(callee, &args)?;
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
    pub fn new(chunk: Rc<Chunk>, env: Env, this_val: Value, args: &[Value]) -> VmCoro {
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
