//! Statement execution, expression evaluation, and the ECMAScript abstract operations. Split out
//! of `interpreter.rs` to keep each file readable; this is the same `impl Interp`.

use crate::ast::*;
use crate::interpreter::*;
use crate::value::*;
use std::rc::Rc;

impl Interp {
    // ----- statements -------------------------------------------------------------------------

    /// Bind a (possibly destructuring) pattern to `value` in `env`. `Var` assigns to the hoisted
    /// binding; `Lexical` creates fresh bindings (params, `let`/`const`).
    pub(crate) fn bind_pattern(
        &mut self,
        pat: &Pattern,
        value: Value,
        env: &Env,
        mode: BindMode,
    ) -> Result<(), Abrupt> {
        match pat {
            Pattern::Ident(name) => {
                match mode {
                    BindMode::Lexical(is_const) => self.init_lexical(name, value, is_const, env),
                    BindMode::Var => self.assign_var(name, value, env)?,
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                // Iterate lazily, pulling only what the pattern needs and closing the iterator when
                // we stop early (or on an abrupt completion).
                let (iter, next) = self.get_iterator(&value)?;
                let mut done = false;
                let result = (|me: &mut Self| -> Result<(), Abrupt> {
                    for el in elems {
                        match el {
                            ArrayPatElem::Hole => {
                                if !done && me.iterator_step(&iter, &next)?.is_none() {
                                    done = true;
                                }
                            }
                            ArrayPatElem::Elem { pattern, default } => {
                                let mut v = if done {
                                    Value::Undefined
                                } else {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => x,
                                        None => {
                                            done = true;
                                            Value::Undefined
                                        }
                                    }
                                };
                                if matches!(v, Value::Undefined) {
                                    if let Some(d) = default {
                                        v = me.eval(d, env)?;
                                        if let (Pattern::Ident(n), true) = (pattern, is_anonymous_fn(d)) {
                                            me.set_fn_name(&v, n);
                                        }
                                    }
                                }
                                me.bind_pattern(pattern, v, env, mode)?;
                            }
                            ArrayPatElem::Rest(pattern) => {
                                let mut rest = Vec::new();
                                while !done {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => rest.push(x),
                                        None => done = true,
                                    }
                                }
                                let arr = me.make_array(rest);
                                me.bind_pattern(pattern, arr, env, mode)?;
                            }
                        }
                    }
                    Ok(())
                })(self);
                if !done {
                    self.iterator_close(&iter);
                }
                result?;
                Ok(())
            }
            Pattern::Object(objpat) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut used: Vec<String> = Vec::new();
                for prop in &objpat.props {
                    let key = self.eval_prop_key(&prop.key, env)?;
                    used.push(key.clone());
                    let mut v = self.get_member(&value, &key)?;
                    if matches!(v, Value::Undefined) {
                        if let Some(d) = &prop.default {
                            v = self.eval(d, env)?;
                            if let (Pattern::Ident(n), true) = (&prop.value, is_anonymous_fn(d)) {
                                self.set_fn_name(&v, n);
                            }
                        }
                    }
                    self.bind_pattern(&prop.value, v, env, mode)?;
                }
                if let Some(rest_name) = &objpat.rest {
                    let obj = self.new_object();
                    if let Value::Obj(src) = &value {
                        let keys: Vec<_> = src
                            .borrow()
                            .props
                            .iter()
                            .filter(|(_, p)| p.enumerable)
                            .map(|(k, _)| k.clone())
                            .collect();
                        for k in keys {
                            if !used.iter().any(|u| u.as_str() == &*k) {
                                let v = self.get_member(&value, &k)?;
                                set_data(&obj, &k, v);
                            }
                        }
                    }
                    self.bind_pattern(&Pattern::Ident(rest_name.clone()), Value::Obj(obj), env, mode)?;
                }
                Ok(())
            }
            // A member target (`o.p`/`o[k]`): assign to it (never a declaration).
            Pattern::Member(target) => self.assign_to_target(target, value, env),
        }
    }

    /// Run a parsed script body in `env` (used by `eval`): hoist, declare lexicals, execute, and
    /// return the completion value (the value of the last value-producing statement).
    pub(crate) fn eval_in_scope(&mut self, body: &[Stmt], env: &Env) -> Result<Value, Abrupt> {
        self.hoist(body, env, true);
        self.declare_block_lexicals(body, env, false);
        let mut last = Value::Undefined;
        for stmt in body {
            let v = self.exec_stmt(stmt, env)?;
            if !matches!(v, Value::Undefined) {
                last = v;
            }
        }
        Ok(last)
    }

    pub fn exec_block(&mut self, stmts: &[Stmt], parent: &Env) -> Completion {
        let scope = new_scope(Some(parent.clone()));
        self.declare_block_lexicals(stmts, &scope, true);
        let mut last = Value::Undefined;
        for s in stmts {
            last = self.exec_stmt(s, &scope)?;
        }
        Ok(last)
    }

    /// Pre-declare `let`/`const` (uninitialised — TDZ) and, when `with_functions`, block-level
    /// function declarations (initialised) for the statements directly in a block.
    pub fn declare_block_lexicals(&mut self, stmts: &[Stmt], scope: &Env, with_functions: bool) {
        for s in stmts {
            match crate::interpreter::unwrap_export(s) {
                Stmt::VarDecl { kind: DeclKind::Let | DeclKind::Const, decls } => {
                    for (pat, _) in decls {
                        let mut names = Vec::new();
                        pattern_idents(pat, &mut names);
                        for name in names {
                            scope.borrow_mut().vars.insert(
                                name,
                                Binding { value: Value::Undefined, mutable: true, initialized: false, import_ref: None },
                            );
                        }
                    }
                }
                Stmt::FuncDecl(func) if with_functions => {
                    if let Some(name) = &func.name {
                        let f = self.make_function(func.clone(), scope.clone());
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding { value: f, mutable: true, initialized: true, import_ref: None },
                        );
                    }
                }
                Stmt::ClassDecl(class) => {
                    if let Some(name) = &class.name {
                        // Classes are lexically scoped with a TDZ until the declaration executes.
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding { value: Value::Undefined, mutable: true, initialized: false, import_ref: None },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    pub fn exec_stmt(&mut self, stmt: &Stmt, env: &Env) -> Completion {
        match stmt {
            Stmt::Empty | Stmt::Debugger | Stmt::FuncDecl(_) => Ok(Value::Undefined),
            Stmt::Expr(e) => self.eval(e, env),
            Stmt::Block(body) => self.exec_block(body, env),
            Stmt::VarDecl { kind, decls } => {
                for (pat, init) in decls {
                    match kind {
                        DeclKind::Var => {
                            // `var x;` (no init) keeps the hoisted binding untouched.
                            if let Some(e) = init {
                                let value = self.eval(e, env)?;
                                if let Pattern::Ident(n) = pat {
                                    if is_anonymous_fn(e) {
                                        self.set_fn_name(&value, n);
                                    }
                                }
                                self.bind_pattern(pat, value, env, BindMode::Var)?;
                            }
                        }
                        DeclKind::Let | DeclKind::Const => {
                            let value = match init {
                                Some(e) => self.eval(e, env)?,
                                None => Value::Undefined,
                            };
                            if let (Pattern::Ident(n), Some(e)) = (pat, init) {
                                if is_anonymous_fn(e) {
                                    self.set_fn_name(&value, n);
                                }
                            }
                            self.bind_pattern(pat, value, env, BindMode::Lexical(*kind == DeclKind::Const))?;
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::Return(arg) => {
                let v = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                Err(Abrupt::Return(v))
            }
            Stmt::Throw(e) => {
                let v = self.eval(e, env)?;
                Err(Abrupt::Throw(v))
            }
            Stmt::If { test, cons, alt } => {
                let t = self.eval(test, env)?;
                if self.to_boolean(&t) {
                    self.exec_stmt(cons, env)
                } else if let Some(a) = alt {
                    self.exec_stmt(a, env)
                } else {
                    Ok(Value::Undefined)
                }
            }
            Stmt::While { test, body } => self.run_loop(None, env, |me, env| {
                let t = me.eval(test, env)?;
                if !me.to_boolean(&t) {
                    return Ok(LoopStep::Done);
                }
                me.exec_stmt(body, env)?;
                Ok(LoopStep::Continue)
            }),
            Stmt::DoWhile { body, test } => {
                let mut first = true;
                self.run_loop(None, env, |me, env| {
                    if !first {
                        let t = me.eval(test, env)?;
                        if !me.to_boolean(&t) {
                            return Ok(LoopStep::Done);
                        }
                    }
                    first = false;
                    me.exec_stmt(body, env)?;
                    let t = me.eval(test, env)?;
                    if !me.to_boolean(&t) {
                        Ok(LoopStep::Done)
                    } else {
                        Ok(LoopStep::Continue)
                    }
                })
            }
            Stmt::For { init, test, update, body } => self.exec_for(init, test, update, body, env, None),
            Stmt::ForInOf { decl, left, right, of, is_await, body } => {
                self.exec_for_in_of(*decl, left, right, *of, *is_await, body, env, None)
            }
            Stmt::Break(label) => Err(Abrupt::Break(label.clone())),
            Stmt::Continue(label) => Err(Abrupt::Continue(label.clone())),
            Stmt::Try { block, handler, finalizer } => self.exec_try(block, handler, finalizer, env),
            Stmt::Switch { disc, cases } => self.exec_switch(disc, cases, env),
            Stmt::Labeled { label, body } => self.exec_labeled(label, body, env),
            Stmt::With { obj, body } => {
                let o = self.eval(obj, env)?;
                if matches!(o, Value::Undefined | Value::Null) {
                    return Err(
                        self.throw("TypeError", "Cannot convert undefined or null to object")
                    );
                }
                let with_env = crate::interpreter::new_with_scope(env.clone(), o);
                self.exec_stmt(body, &with_env)
            }
            Stmt::ClassDecl(class) => {
                let value = self.eval_class(class, env)?;
                if let Some(name) = &class.name {
                    self.init_lexical(name, value, false, env);
                }
                Ok(Value::Undefined)
            }
            // Module declarations: imports are resolved at link time (runtime no-op); exports run
            // their inner declaration (the export itself is link-time metadata).
            Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => Ok(Value::Undefined),
            Stmt::ExportDecl(inner) => self.exec_stmt(inner, env),
            Stmt::ExportDefault(inner) => match &**inner {
                Stmt::Expr(e) => {
                    let v = self.eval(e, env)?;
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Undefined)
                }
                other => self.exec_stmt(other, env),
            },
        }
    }

    fn exec_labeled(&mut self, label: &str, body: &Stmt, env: &Env) -> Completion {
        // For loops, push the label so labeled break/continue can target them.
        let result = match body {
            Stmt::For { init, test, update, body } => {
                self.exec_for(init, test, update, body, env, Some(label))
            }
            Stmt::ForInOf { decl, left, right, of, is_await, body } => {
                self.exec_for_in_of(*decl, left, right, *of, *is_await, body, env, Some(label))
            }
            Stmt::While { .. } | Stmt::DoWhile { .. } => self.exec_stmt(body, env),
            other => self.exec_stmt(other, env),
        };
        match result {
            Err(Abrupt::Break(Some(l))) if l == label => Ok(Value::Undefined),
            other => other,
        }
    }

    fn run_loop(
        &mut self,
        label: Option<&str>,
        env: &Env,
        mut step: impl FnMut(&mut Interp, &Env) -> Result<LoopStep, Abrupt>,
    ) -> Completion {
        loop {
            // Safe point: run the cycle collector / enforce the live-object ceiling. Catches tight
            // loops (`for(;;){ x = {}; }`) that never call a function.
            self.gc_check()?;
            match step(self, env) {
                Ok(LoopStep::Continue) => {}
                Ok(LoopStep::Done) => return Ok(Value::Undefined),
                Err(Abrupt::Break(None)) => return Ok(Value::Undefined),
                Err(Abrupt::Break(Some(l))) if Some(l.as_str()) == label => {
                    return Ok(Value::Undefined)
                }
                Err(Abrupt::Continue(None)) => {}
                Err(Abrupt::Continue(Some(l))) if Some(l.as_str()) == label => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn exec_for(
        &mut self,
        init: &Option<Box<ForInit>>,
        test: &Option<Expr>,
        update: &Option<Expr>,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Completion {
        let loop_env = new_scope(Some(env.clone()));
        if let Some(init) = init {
            match init.as_ref() {
                ForInit::Expr(e) => {
                    self.eval(e, &loop_env)?;
                }
                ForInit::VarDecl { kind, decls } => {
                    if matches!(kind, DeclKind::Let | DeclKind::Const) {
                        for (pat, _) in decls {
                            let mut names = Vec::new();
                            pattern_idents(pat, &mut names);
                            for name in names {
                                loop_env.borrow_mut().vars.insert(
                                    name,
                                    Binding { value: Value::Undefined, mutable: true, initialized: false, import_ref: None },
                                );
                            }
                        }
                    }
                    let mode = match kind {
                        DeclKind::Var => BindMode::Var,
                        k => BindMode::Lexical(*k == DeclKind::Const),
                    };
                    for (pat, e) in decls {
                        let v = match e {
                            Some(e) => self.eval(e, &loop_env)?,
                            None => Value::Undefined,
                        };
                        self.bind_pattern(pat, v, &loop_env, mode)?;
                    }
                }
            }
        }
        let mut first = true;
        self.run_loop(label, &loop_env, |me, env| {
            if !first {
                if let Some(u) = update {
                    me.eval(u, env)?;
                }
            }
            first = false;
            if let Some(t) = test {
                let tv = me.eval(t, env)?;
                if !me.to_boolean(&tv) {
                    return Ok(LoopStep::Done);
                }
            }
            me.exec_stmt(body, env)?;
            Ok(LoopStep::Continue)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for_in_of(
        &mut self,
        decl: Option<DeclKind>,
        left: &Pattern,
        right: &Expr,
        of: bool,
        is_await: bool,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Completion {
        let rhs = self.eval(right, env)?;
        // No-decl form assigns to an existing binding; a declaration creates a fresh one per round.
        let mode = match decl {
            Some(DeclKind::Var) | None => BindMode::Var,
            Some(k) => BindMode::Lexical(k == DeclKind::Const),
        };
        if of && is_await {
            // `for await (x of asyncIterable)`: drive the @@asyncIterator (or a sync iterator over
            // promises), awaiting each `next()` result and value.
            let akey = crate::builtins::async_iterator_key(self);
            let method = match &akey {
                Some(k) => self.get_member(&rhs, k)?,
                None => Value::Undefined,
            };
            let iter = if method.is_callable() {
                self.call(method, rhs.clone(), &[])?
            } else {
                self.get_iterator(&rhs)?.0
            };
            let next = self.get_member(&iter, "next")?;
            return self.run_loop(label, env, |me, env| {
                let res = me.call(next.clone(), iter.clone(), &[])?;
                let res = me.await_value(res)?;
                let done = me.get_member(&res, "done")?;
                if me.to_boolean(&done) {
                    return Ok(LoopStep::Done);
                }
                let raw = me.get_member(&res, "value")?;
                let v = me.await_value(raw)?;
                let iter_env = new_scope(Some(env.clone()));
                me.bind_pattern(left, v, &iter_env, mode)?;
                me.exec_stmt(body, &iter_env)?;
                Ok(LoopStep::Continue)
            });
        }
        if of {
            // Step the iterator lazily; close it if the loop exits early (break/return/throw).
            let (iter, next) = self.get_iterator(&rhs)?;
            let iter_close = iter.clone();
            let mut exhausted = false;
            let result = self.run_loop(label, env, |me, env| {
                let v = match me.iterator_step(&iter, &next)? {
                    Some(x) => x,
                    None => {
                        exhausted = true;
                        return Ok(LoopStep::Done);
                    }
                };
                let iter_env = new_scope(Some(env.clone()));
                me.bind_pattern(left, v, &iter_env, mode)?;
                me.exec_stmt(body, &iter_env)?;
                Ok(LoopStep::Continue)
            });
            if !exhausted {
                self.iterator_close(&iter_close);
            }
            return result;
        }
        // for-in: enumerable string keys along the prototype chain (own first, deduped).
        let items: Vec<Value> = self.enum_keys(&rhs).into_iter().map(Value::from_string).collect();
        let mut idx = 0;
        self.run_loop(label, env, |me, env| {
            if idx >= items.len() {
                return Ok(LoopStep::Done);
            }
            let v = items[idx].clone();
            idx += 1;
            let iter_env = new_scope(Some(env.clone()));
            me.bind_pattern(left, v, &iter_env, mode)?;
            me.exec_stmt(body, &iter_env)?;
            Ok(LoopStep::Continue)
        })
    }

    /// GetIterator: returns (iterator, next-method).
    pub(crate) fn get_iterator(&mut self, v: &Value) -> Result<(Value, Value), Abrupt> {
        // A primitive string iterates by code point (it has no own @@iterator method here).
        if let Value::Str(s) = v {
            let chars: Vec<Value> = s.chars().map(|c| Value::from_string(c.to_string())).collect();
            let arr = self.make_array(chars);
            return self.get_iterator(&arr);
        }
        let key = match &self.iterator_sym {
            Some(s) => Interp::sym_key(s),
            None => return Err(self.throw("TypeError", "no iterator symbol")),
        };
        let itfn = self.get_member(v, &key)?;
        if !itfn.is_callable() {
            return Err(self.throw("TypeError", "value is not iterable"));
        }
        let iter = self.call(itfn, v.clone(), &[])?;
        let next = self.get_member(&iter, "next")?;
        if !next.is_callable() {
            return Err(self.throw("TypeError", "iterator.next is not a function"));
        }
        Ok((iter, next))
    }
    /// IteratorStep: `Some(value)` or `None` when done.
    pub(crate) fn iterator_step(&mut self, iter: &Value, next: &Value) -> Result<Option<Value>, Abrupt> {
        let res = self.call(next.clone(), iter.clone(), &[])?;
        if !matches!(res, Value::Obj(_)) {
            return Err(self.throw("TypeError", "iterator result is not an object"));
        }
        let done = self.get_member(&res, "done")?;
        if self.to_boolean(&done) {
            Ok(None)
        } else {
            Ok(Some(self.get_member(&res, "value")?))
        }
    }
    /// IteratorClose: call `return()` if present (swallowing its result/most errors).
    pub(crate) fn iterator_close(&mut self, iter: &Value) {
        if let Ok(ret) = self.get_member(iter, "return") {
            if ret.is_callable() {
                let _ = self.call(ret, iter.clone(), &[]);
            }
        }
    }

    /// Collect every value an iterable yields. Strings and plain arrays use a fast path; everything
    /// else goes through the `Symbol.iterator` protocol (call `@@iterator`, then drain `.next()`).
    pub(crate) fn iterate(&mut self, v: &Value) -> Result<Vec<Value>, Abrupt> {
        match v {
            Value::Str(s) => {
                return Ok(s.chars().map(|c| Value::from_string(c.to_string())).collect())
            }
            Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array) => {
                let len = self.checked_array_len(o)?;
                let mut out = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    out.push(self.get_member(v, &i.to_string())?);
                }
                return Ok(out);
            }
            _ => {}
        }
        // General iterator protocol.
        if let Some(sym) = self.iterator_sym.clone() {
            let key = Interp::sym_key(&sym);
            let itfn = self.get_member(v, &key)?;
            if itfn.is_callable() {
                let iter = self.call(itfn, v.clone(), &[])?;
                let next = self.get_member(&iter, "next")?;
                if !next.is_callable() {
                    return Err(self.throw("TypeError", "iterator.next is not a function"));
                }
                let mut out = Vec::new();
                loop {
                    let res = self.call(next.clone(), iter.clone(), &[])?;
                    if !matches!(res, Value::Obj(_)) {
                        return Err(self.throw("TypeError", "iterator result is not an object"));
                    }
                    let done = self.get_member(&res, "done")?;
                    if self.to_boolean(&done) {
                        break;
                    }
                    out.push(self.get_member(&res, "value")?);
                    if out.len() > crate::interpreter::MAX_ARRAY_OP_LEN {
                        return Err(self.throw("RangeError", "iterator produced too many values"));
                    }
                }
                return Ok(out);
            }
        }
        Err(self.throw("TypeError", "value is not iterable"))
    }

    /// Whether `v` is iterable (has a callable `@@iterator`). Used by `Array.from` to choose between
    /// the iterator protocol and the array-like fallback.
    pub(crate) fn has_iterator(&mut self, v: &Value) -> bool {
        if let Some(sym) = self.iterator_sym.clone() {
            let key = Interp::sym_key(&sym);
            if let Ok(f) = self.get_member(v, &key) {
                return f.is_callable();
            }
        }
        false
    }

    fn enum_keys(&self, v: &Value) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut cur = match v {
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        };
        while let Some(o) = cur {
            // for-in visits own enumerable string keys in spec order, then up the prototype chain.
            let b = o.borrow();
            for k in b.props.ordered_keys() {
                if Interp::is_sym_key(&k) {
                    continue;
                }
                let enumerable = b.props.get(&k).map(|p| p.enumerable).unwrap_or(false);
                if enumerable && seen.insert(k.to_string()) {
                    out.push(k.to_string());
                }
            }
            let parent = b.proto.clone();
            drop(b);
            cur = parent;
        }
        out
    }

    fn exec_try(
        &mut self,
        block: &[Stmt],
        handler: &Option<(Option<Pattern>, Vec<Stmt>)>,
        finalizer: &Option<Vec<Stmt>>,
        env: &Env,
    ) -> Completion {
        let result = self.exec_block(block, env);
        let after_catch = match result {
            Err(Abrupt::Throw(ex)) => {
                if let Some((param, body)) = handler {
                    let catch_env = new_scope(Some(env.clone()));
                    if let Some(pat) = param {
                        self.bind_pattern(pat, ex, &catch_env, BindMode::Lexical(false))?;
                    }
                    let mut last = Ok(Value::Undefined);
                    self.declare_block_lexicals(body, &catch_env, true);
                    for s in body {
                        match self.exec_stmt(s, &catch_env) {
                            Ok(v) => last = Ok(v),
                            Err(e) => {
                                last = Err(e);
                                break;
                            }
                        }
                    }
                    last
                } else {
                    Err(Abrupt::Throw(ex))
                }
            }
            other => other,
        };
        if let Some(fin) = finalizer {
            // An abrupt completion in `finally` overrides the try/catch completion; its normal
            // value is discarded (the try/catch completion stands).
            self.exec_block(fin, env)?;
        }
        after_catch
    }

    fn exec_switch(&mut self, disc: &Expr, cases: &[SwitchCase], env: &Env) -> Completion {
        let d = self.eval(disc, env)?;
        let scope = new_scope(Some(env.clone()));
        for case in cases {
            for s in &case.body {
                self.declare_block_lexicals(std::slice::from_ref(s), &scope, true);
            }
        }
        let mut matched = None;
        for (i, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                let t = self.eval(test, &scope)?;
                if self.strict_equals(&d, &t) {
                    matched = Some(i);
                    break;
                }
            }
        }
        let start = match matched.or_else(|| cases.iter().position(|c| c.test.is_none())) {
            Some(i) => i,
            None => return Ok(Value::Undefined),
        };
        let mut last = Value::Undefined;
        for case in &cases[start..] {
            for s in &case.body {
                match self.exec_stmt(s, &scope) {
                    Ok(v) => last = v,
                    Err(Abrupt::Break(None)) => return Ok(last),
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(last)
    }

    // ----- variable binding -------------------------------------------------------------------

    fn init_lexical(&mut self, name: &str, value: Value, is_const: bool, env: &Env) {
        env.borrow_mut().vars.insert(
            name.to_string(),
            Binding { value, mutable: !is_const, initialized: true, import_ref: None },
        );
    }

    /// Whether `name` resolves to a declared-but-uninitialized (TDZ) lexical binding.
    fn binding_in_tdz(&self, name: &str, env: &Env) -> bool {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let b = s.borrow();
            if let Some(binding) = b.vars.get(name) {
                return !binding.initialized;
            }
            cur = b.parent.clone();
        }
        false
    }

    pub fn get_var(&mut self, name: &str, env: &Env) -> Result<Value, Abrupt> {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let (with_obj, parent) = {
                let b = s.borrow();
                if let Some(binding) = b.vars.get(name) {
                    if !binding.initialized {
                        return Err(self.throw(
                            "ReferenceError",
                            format!("cannot access '{name}' before initialization"),
                        ));
                    }
                    // A live module import reads through to the exporter's binding.
                    if let Some((src_env, local)) = &binding.import_ref {
                        let (src_env, local) = (src_env.clone(), local.clone());
                        drop(b);
                        return self.get_var(&local, &src_env);
                    }
                    return Ok(binding.value.clone());
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            // `with (obj)`: resolve against the object's properties if it has the name.
            if let Some(obj @ Value::Obj(o)) = &with_obj {
                if self.has_property(o, name) {
                    return self.get_member(obj, name);
                }
            }
            cur = parent;
        }
        // Fall back to a property of the global object (where builtins live).
        let g = Value::Obj(self.global.clone());
        if self.has_property(&self.global.clone(), name) {
            return self.get_member(&g, name);
        }
        Err(self.throw("ReferenceError", format!("{name} is not defined")))
    }

    pub fn assign_var(&mut self, name: &str, value: Value, env: &Env) -> Result<(), Abrupt> {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let (with_obj, parent) = {
                let mut b = s.borrow_mut();
                if let Some(binding) = b.vars.get_mut(name) {
                    if !binding.mutable && binding.initialized {
                        return Err(
                            self.throw("TypeError", format!("assignment to constant '{name}'"))
                        );
                    }
                    binding.value = value;
                    binding.initialized = true;
                    return Ok(());
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            if let Some(obj @ Value::Obj(o)) = &with_obj {
                if self.has_property(o, name) {
                    return self.set_member(obj, name, value);
                }
            }
            cur = parent;
        }
        // A declared global `var`/`function` lives as a property of the global object.
        if self.has_property(&self.global.clone(), name) {
            return self.set_member(&Value::Obj(self.global.clone()), name, value);
        }
        // Truly undeclared: strict → ReferenceError; sloppy → create a global property.
        if self.strict {
            return Err(self.throw("ReferenceError", format!("{name} is not defined")));
        }
        let g = Value::Obj(self.global.clone());
        self.set_member(&g, name, value)
    }

    pub(crate) fn has_property(&self, obj: &Gc, key: &str) -> bool {
        // TypedArray elements live in the backing buffer, not the property map.
        if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(obj) as usize)) {
            if let Ok(idx) = key.parse::<usize>() {
                return idx < info.len;
            }
        }
        let mut cur = Some(obj.clone());
        while let Some(o) = cur {
            if o.borrow().props.contains(key) {
                return true;
            }
            cur = o.borrow().proto.clone();
        }
        false
    }

    // ----- expressions ------------------------------------------------------------------------

    pub fn eval(&mut self, expr: &Expr, env: &Env) -> Result<Value, Abrupt> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::BigInt(n) => Ok(Value::BigInt(*n)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => self.get_var(name, env),
            Expr::This => self.get_var("this", env).or(Ok(Value::Undefined)),
            Expr::Regex { body, flags } => self.make_regexp(body, flags),
            Expr::Array(elems) => self.eval_array(elems, env),
            Expr::Object(props) => self.eval_object(props, env),
            Expr::Func(func) => Ok(self.make_function(func.clone(), env.clone())),
            Expr::Class(class) => self.eval_class(class, env),
            Expr::Yield { delegate, arg } => {
                let value = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                if self.yield_buffer.is_none() {
                    return Err(self.throw("SyntaxError", "yield outside a generator"));
                }
                // Past the buffer cap, abort the body (an infinite generator can't be modeled
                // eagerly). The thrown error stops execution and surfaces from the generator.
                if self.yield_buffer.as_ref().map(|b| b.len()).unwrap_or(0) >= 5_000 {
                    return Err(self.throw("RangeError", "generator produced too many values"));
                }
                if *delegate {
                    let items = self.iterate(&value)?;
                    if let Some(buf) = &mut self.yield_buffer {
                        buf.extend(items.into_iter().take(5_000 - buf.len()));
                    }
                } else if let Some(buf) = &mut self.yield_buffer {
                    buf.push(value);
                }
                Ok(Value::Undefined)
            }
            Expr::Await(e) => {
                let v = self.eval(e, env)?;
                self.await_value(v)
            }
            Expr::Super => Err(self.throw("SyntaxError", "'super' keyword unexpected here")),
            Expr::Seq(items) => {
                let mut last = Value::Undefined;
                for e in items {
                    last = self.eval(e, env)?;
                }
                Ok(last)
            }
            Expr::Cond { test, cons, alt } => {
                let t = self.eval(test, env)?;
                if self.to_boolean(&t) {
                    self.eval(cons, env)
                } else {
                    self.eval(alt, env)
                }
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval(left, env)?;
                match *op {
                    "&&" => {
                        if self.to_boolean(&l) {
                            self.eval(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                    "||" => {
                        if self.to_boolean(&l) {
                            Ok(l)
                        } else {
                            self.eval(right, env)
                        }
                    }
                    "??" => {
                        if matches!(l, Value::Undefined | Value::Null) {
                            self.eval(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Expr::Unary { op, arg } => self.eval_unary(op, arg, env),
            Expr::Update { op, prefix, arg } => self.eval_update(op, *prefix, arg, env),
            Expr::Binary { op, left, right } => {
                let l = self.eval(left, env)?;
                let r = self.eval(right, env)?;
                self.binary(op, l, r)
            }
            Expr::Assign { op, target, value } => self.eval_assign(op, target, value, env),
            Expr::ImportMeta => Ok(self.import_meta.clone().unwrap_or(Value::Undefined)),
            Expr::ImportCall(spec) => {
                let specifier = self.eval(spec, env)?;
                let s = self.to_string(&specifier)?;
                Ok(self.dynamic_import(&s))
            }
            Expr::PrivateIn { name, obj } => {
                let o = self.eval(obj, env)?;
                match o {
                    // Private fields are own props; private methods live on the prototype.
                    Value::Obj(obj) => Ok(Value::Bool(self.has_property(&obj, name))),
                    _ => Err(self.throw("TypeError", "the right-hand side of 'in' must be an object")),
                }
            }
            Expr::OptionalChain(inner) => {
                let saved = self.short_circuit;
                self.short_circuit = false;
                let v = self.eval(inner, env);
                let short = self.short_circuit;
                self.short_circuit = saved;
                if short {
                    Ok(Value::Undefined)
                } else {
                    v
                }
            }
            Expr::Member { obj, prop, optional } => {
                if matches!(**obj, Expr::Super) {
                    let home = self.get_var("%superproto%", env)?;
                    return self.get_member(&home, prop);
                }
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined); // an earlier `?.` link short-circuited
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                self.get_member(&base, prop)
            }
            Expr::Index { obj, index, optional } => {
                if matches!(**obj, Expr::Super) {
                    let home = self.get_var("%superproto%", env)?;
                    let idx = self.eval(index, env)?;
                    let key = self.to_property_key(&idx)?;
                    return self.get_member(&home, &key);
                }
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                self.get_member(&base, &key)
            }
            Expr::Call { callee, args, optional } => self.eval_call(callee, args, *optional, env),
            Expr::TaggedTemplate { tag, quasis, subs } => {
                self.eval_tagged_template(tag, quasis, subs, env)
            }
            Expr::New { callee, args } => {
                let c = self.eval(callee, env)?;
                let argv = self.eval_args(args, env)?;
                self.construct(c, &argv)
            }
        }
    }

    fn eval_array(&mut self, elems: &[ArrayElem], env: &Env) -> Result<Value, Abrupt> {
        // Holes leave the index absent (a real elision), not `undefined`.
        let arr = self.make_array(Vec::new());
        let mut idx: usize = 0;
        for e in elems {
            match e {
                ArrayElem::Item(e) => {
                    let v = self.eval(e, env)?;
                    self.set_member(&arr, &idx.to_string(), v)?;
                    idx += 1;
                }
                ArrayElem::Hole => idx += 1,
                ArrayElem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    for item in self.iterate(&v)? {
                        self.set_member(&arr, &idx.to_string(), item)?;
                        idx += 1;
                    }
                }
            }
        }
        self.set_member(&arr, "length", Value::Num(idx as f64))?;
        Ok(arr)
    }

    /// NamedEvaluation: give an anonymous function/class the binding/property name it's assigned to,
    /// unless it already has a non-empty name.
    pub(crate) fn set_fn_name(&mut self, v: &Value, name: &str) {
        if !v.is_callable() {
            return;
        }
        if let Value::Obj(o) = v {
            let empty = o
                .borrow()
                .props
                .get("name")
                .map(|p| matches!(&p.value, Value::Str(s) if s.is_empty()))
                .unwrap_or(true);
            if empty {
                o.borrow_mut().props.insert(
                    "name".to_string(),
                    Property::data(Value::from_string(name.to_string()), false, false, true),
                );
            }
        }
    }

    fn eval_object(&mut self, props: &[PropDef], env: &Env) -> Result<Value, Abrupt> {
        let obj = self.new_object();
        for prop in props {
            match prop {
                PropDef::KeyValue { key, value } => {
                    let k = self.eval_prop_key(key, env)?;
                    let v = self.eval(value, env)?;
                    if is_anonymous_fn(value) {
                        self.set_fn_name(&v, &k);
                    }
                    obj.borrow_mut().props.insert(k, Property::plain(v));
                }
                PropDef::Getter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), env.clone());
                    self.set_fn_name(&f, &format!("get {k}"));
                    self.define_accessor(&obj, &k, Some(f), None);
                }
                PropDef::Setter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), env.clone());
                    self.set_fn_name(&f, &format!("set {k}"));
                    self.define_accessor(&obj, &k, None, Some(f));
                }
                PropDef::Spread(e) => {
                    let v = self.eval(e, env)?;
                    if let Value::Obj(src) = &v {
                        let keys: Vec<Rc<str>> = {
                            let b = src.borrow();
                            b.props
                                .ordered_keys()
                                .into_iter()
                                .filter(|k| b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
                                .collect()
                        };
                        for k in keys {
                            let pv = self.get_member(&v, &k)?;
                            obj.borrow_mut().props.insert(k, Property::plain(pv));
                        }
                    }
                }
            }
        }
        Ok(Value::Obj(obj))
    }

    fn define_accessor(&self, obj: &Gc, key: &str, get: Option<Value>, set: Option<Value>) {
        let mut b = obj.borrow_mut();
        if let Some(p) = b.props.get_mut(key) {
            if p.accessor {
                if get.is_some() {
                    p.get = get;
                }
                if set.is_some() {
                    p.set = set;
                }
                return;
            }
        }
        b.props.insert(
            key,
            Property {
                value: Value::Undefined,
                get,
                set,
                accessor: true,
                writable: false,
                enumerable: true,
                configurable: true,
            },
        );
    }

    fn eval_prop_key(&mut self, key: &PropKey, env: &Env) -> Result<String, Abrupt> {
        match key {
            PropKey::Ident(s) => Ok(s.clone()),
            PropKey::Str(s) => Ok(s.to_string()),
            PropKey::Num(n) => Ok(self.num_to_str(*n)),
            PropKey::Computed(e) => {
                let v = self.eval(e, env)?;
                self.to_property_key(&v)
            }
        }
    }

    fn eval_args(&mut self, args: &[ArrayElem], env: &Env) -> Result<Vec<Value>, Abrupt> {
        let mut out = Vec::new();
        for a in args {
            match a {
                ArrayElem::Item(e) => out.push(self.eval(e, env)?),
                ArrayElem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    out.extend(self.iterate(&v)?);
                }
                ArrayElem::Hole => out.push(Value::Undefined),
            }
        }
        Ok(out)
    }

    /// Freeze an object in place (non-extensible, all own props non-writable/non-configurable).
    pub(crate) fn freeze_object(&self, v: &Value) {
        if let Value::Obj(o) = v {
            o.borrow_mut().extensible = false;
            let keys = o.borrow().props.keys();
            for k in keys {
                if let Some(p) = o.borrow_mut().props.get_mut(&k) {
                    p.writable = false;
                    p.configurable = false;
                }
            }
        }
    }

    fn eval_tagged_template(
        &mut self,
        tag: &Expr,
        quasis: &[(Option<String>, String)],
        subs: &[Expr],
        env: &Env,
    ) -> Result<Value, Abrupt> {
        // The template object: a frozen array of cooked strings with a frozen `.raw` array.
        let cooked: Vec<Value> = quasis
            .iter()
            .map(|(c, _)| c.as_ref().map(|s| Value::from_string(s.clone())).unwrap_or(Value::Undefined))
            .collect();
        let raw: Vec<Value> = quasis.iter().map(|(_, r)| Value::from_string(r.clone())).collect();
        let strings = self.make_array(cooked);
        let raw_arr = self.make_array(raw);
        self.freeze_object(&raw_arr);
        self.set_member(&strings, "raw", raw_arr)?;
        self.freeze_object(&strings);
        // Evaluate the tag callee, capturing `this` for method tags (`obj.tag\`...\``).
        let (func, this) = match tag {
            Expr::Member { obj, prop, .. } => {
                let base = self.eval(obj, env)?;
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            Expr::Index { obj, index, .. } => {
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&base, &key)?;
                (f, base)
            }
            _ => (self.eval(tag, env)?, Value::Undefined),
        };
        if !func.is_callable() {
            return Err(self.throw("TypeError", "tag is not a function"));
        }
        let mut argv = vec![strings];
        for s in subs {
            argv.push(self.eval(s, env)?);
        }
        self.call(func, this, &argv)
    }

    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[ArrayElem],
        optional: bool,
        env: &Env,
    ) -> Result<Value, Abrupt> {
        // Direct eval: `eval(src)` called by that exact name runs the code in the *caller's* scope
        // (so it can see/define local bindings). Any other way of reaching eval is indirect and runs
        // in the global scope (handled by the global `eval` native).
        if let Expr::Ident(name) = callee {
            if name == "eval" {
                if let (Ok(Value::Obj(f)), Some(ef)) = (self.get_var("eval", env), self.eval_fn.clone())
                {
                    if Rc::ptr_eq(&f, &ef) {
                        let argv = self.eval_args(args, env)?;
                        return self.direct_eval(argv.first(), env);
                    }
                }
            }
        }
        // `super(...)`: invoke the parent constructor on the current `this`, then run this class's
        // instance-field initializers.
        if matches!(callee, Expr::Super) {
            let parent = self.get_var("%superclass%", env)?;
            if matches!(parent, Value::Undefined) {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            }
            let this = self.get_var("this", env)?;
            let argv = self.eval_args(args, env)?;
            self.run_constructor_on(&parent, &this, &argv)?;
            let this_ctor = self.get_var("%thisctor%", env)?;
            self.init_instance_fields(&this_ctor, &this)?;
            return Ok(Value::Undefined);
        }
        // `super.m(...)` / `super[k](...)`: method on the super prototype, called with current `this`.
        if let Expr::Member { obj, prop, .. } = callee {
            if matches!(**obj, Expr::Super) {
                let home = self.get_var("%superproto%", env)?;
                let f = self.get_member(&home, prop)?;
                let this = self.get_var("this", env)?;
                let argv = self.eval_args(args, env)?;
                return self.call(f, this, &argv);
            }
        }
        if let Expr::Index { obj, index, .. } = callee {
            if matches!(**obj, Expr::Super) {
                let home = self.get_var("%superproto%", env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&home, &key)?;
                let this = self.get_var("this", env)?;
                let argv = self.eval_args(args, env)?;
                return self.call(f, this, &argv);
            }
        }
        // Determine `this` for method calls (`obj.m()` → this = obj).
        let (func, this) = match callee {
            Expr::Member { obj, prop, optional } => {
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            Expr::Index { obj, index, optional } => {
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&base, &key)?;
                (f, base)
            }
            _ => {
                let f = self.eval(callee, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                (f, Value::Undefined)
            }
        };
        // `f?.()` short-circuits the whole chain when the callee is nullish.
        if optional && matches!(func, Value::Undefined | Value::Null) {
            self.short_circuit = true;
            return Ok(Value::Undefined);
        }
        let argv = self.eval_args(args, env)?;
        if !func.is_callable() {
            let desc = describe_callee(callee);
            return Err(self.throw("TypeError", format!("{desc} is not a function")));
        }
        self.call(func, this, &argv)
    }

    /// `await v`: if `v` is a promise, drain microtasks to settle it, then return its value (or
    /// throw its reason). A non-promise is returned as-is. A still-pending promise yields undefined
    /// (lumen cannot truly suspend).
    pub(crate) fn await_value(&mut self, v: Value) -> Result<Value, Abrupt> {
        let ptr = match &v {
            Value::Obj(o) if self.promises.contains_key(&(Rc::as_ptr(o) as usize)) => {
                Rc::as_ptr(o) as usize
            }
            _ => return Ok(v),
        };
        self.drain_microtasks();
        match self.promises.get(&ptr) {
            Some(s) if s.status == 1 => Ok(s.value.clone()),
            Some(s) if s.status == 2 => Err(Abrupt::Throw(s.value.clone())),
            _ => Ok(Value::Undefined),
        }
    }

    // ----- promises ---------------------------------------------------------------------------

    pub(crate) fn new_promise(&mut self) -> Value {
        let obj = Object::new(self.extra_protos.get("Promise").cloned());
        let p = Rc::as_ptr(&obj) as usize;
        self.promises.insert(p, PromiseState::default());
        Value::Obj(obj)
    }

    /// A bound function that settles `promise` (fulfilling or rejecting) when called.
    pub(crate) fn make_resolver(&mut self, promise: &Value, fulfilling: bool) -> Value {
        let target = self.make_native(
            if fulfilling { "resolve" } else { "reject" },
            1,
            if fulfilling { promise_resolve_native } else { promise_reject_native },
        );
        let bound = Object::new(Some(self.function_proto.clone()));
        bound.borrow_mut().call =
            Callable::Bound { target, this: promise.clone(), args: Vec::new() };
        Value::Obj(bound)
    }

    pub(crate) fn resolve_promise(&mut self, promise: &Value, value: Value) {
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        if self.promises.get(&ptr).map(|s| s.status).unwrap_or(1) != 0 {
            return;
        }
        // Adopt a thenable's eventual state.
        if matches!(value, Value::Obj(_)) {
            if let Ok(then) = self.get_member(&value, "then") {
                if then.is_callable() {
                    let res = self.make_resolver(promise, true);
                    let rej = self.make_resolver(promise, false);
                    if let Err(Abrupt::Throw(e)) = self.call(then, value.clone(), &[res, rej]) {
                        self.reject_promise(promise, e);
                    }
                    return;
                }
            }
        }
        self.settle(promise, value, true);
    }

    pub(crate) fn reject_promise(&mut self, promise: &Value, reason: Value) {
        self.settle(promise, reason, false);
    }

    fn settle(&mut self, promise: &Value, value: Value, fulfilled: bool) {
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        let reactions = match self.promises.get_mut(&ptr) {
            Some(s) if s.status == 0 => {
                s.status = if fulfilled { 1 } else { 2 };
                s.value = value.clone();
                std::mem::take(&mut s.reactions)
            }
            _ => return,
        };
        for (on_f, on_r, result) in reactions {
            let handler = if fulfilled { on_f } else { on_r };
            self.microtasks.push_back(Job { handler, result, value: value.clone(), fulfilled });
        }
    }

    /// The `then` operation: register reactions, returning a new dependent promise.
    pub(crate) fn promise_then(&mut self, promise: &Value, on_f: Value, on_r: Value) -> Value {
        let result = self.new_promise();
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return result,
        };
        let status = self.promises.get(&ptr).map(|s| s.status).unwrap_or(0);
        match status {
            0 => {
                if let Some(s) = self.promises.get_mut(&ptr) {
                    s.reactions.push((on_f, on_r, result.clone()));
                }
            }
            1 => {
                let v = self.promises[&ptr].value.clone();
                self.microtasks.push_back(Job { handler: on_f, result: result.clone(), value: v, fulfilled: true });
            }
            _ => {
                let v = self.promises[&ptr].value.clone();
                self.microtasks.push_back(Job { handler: on_r, result: result.clone(), value: v, fulfilled: false });
            }
        }
        result
    }

    /// Drain the microtask queue (called after the main script). Bounded to avoid an unbounded loop.
    pub(crate) fn drain_microtasks(&mut self) {
        let mut budget = 100_000u32;
        while let Some(job) = self.microtasks.pop_front() {
            budget -= 1;
            if budget == 0 {
                self.microtasks.clear();
                break;
            }
            if job.handler.is_callable() {
                match self.call(job.handler.clone(), Value::Undefined, std::slice::from_ref(&job.value)) {
                    Ok(r) => self.resolve_promise(&job.result, r),
                    Err(Abrupt::Throw(e)) => self.reject_promise(&job.result, e),
                    Err(_) => {}
                }
            } else if job.fulfilled {
                self.resolve_promise(&job.result, job.value);
            } else {
                self.reject_promise(&job.result, job.value);
            }
        }
    }

    /// Compile a regular expression and build a RegExp object (its metadata stored as own props,
    /// the compiled program in the `regexps` side table). A bad pattern throws a SyntaxError.
    pub(crate) fn make_regexp(&mut self, source: &str, flags: &str) -> Result<Value, Abrupt> {
        let re = crate::regex::Regex::new(source, flags).map_err(|e| self.throw("SyntaxError", e))?;
        let obj = Object::new(self.extra_protos.get("RegExp").cloned());
        let ptr = Rc::as_ptr(&obj) as usize;
        // source/flags/global/... are accessor getters on RegExp.prototype (computed from the
        // matcher); only `lastIndex` is an own writable data property.
        obj.borrow_mut()
            .props
            .insert("lastIndex", Property::data(Value::Num(0.0), true, false, false));
        self.regexps.insert(ptr, Rc::new(re));
        Ok(Value::Obj(obj))
    }

    /// Direct eval: a non-string argument is returned unchanged; a string is parsed and executed.
    /// Strict eval (inherited or via its own `"use strict"`) gets a fresh scope; sloppy eval shares
    /// the caller's scope so `var`/function declarations leak into it (per spec for sloppy code).
    fn direct_eval(&mut self, arg: Option<&Value>, env: &Env) -> Result<Value, Abrupt> {
        let code = match arg {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => return Ok(other.clone()),
            None => return Ok(Value::Undefined),
        };
        let body = crate::parser::parse_script(&code, self.strict)
            .map_err(|e| self.throw("SyntaxError", e.message))?;
        let directive_strict = matches!(
            body.first(),
            Some(Stmt::Expr(Expr::Str(s))) if &**s == "use strict"
        );
        let run_env = if self.strict || directive_strict {
            new_scope(Some(env.clone()))
        } else {
            env.clone()
        };
        let saved = self.strict;
        self.strict = self.strict || directive_strict;
        let result = self.eval_in_scope(&body, &run_env);
        self.strict = saved;
        result
    }

    // ----- classes ----------------------------------------------------------------------------

    fn eval_class(&mut self, class: &Rc<Class>, env: &Env) -> Result<Value, Abrupt> {
        // Superclass and the prototype / static parents it implies.
        let parent = match &class.superclass {
            Some(e) => Some(self.eval(e, env)?),
            None => None,
        };
        let (proto_parent, ctor_parent): (Option<Gc>, Option<Value>) = match &parent {
            None => (Some(self.object_proto.clone()), None),
            Some(Value::Null) => (None, None),
            Some(v @ Value::Obj(pc)) if v.is_callable() => {
                let pp = self.get_member(v, "prototype")?;
                let pp = match pp {
                    Value::Obj(o) => Some(o),
                    Value::Null => None,
                    _ => {
                        return Err(self.throw(
                            "TypeError",
                            "Class extends value does not have a valid prototype property",
                        ))
                    }
                };
                (pp, Some(Value::Obj(pc.clone())))
            }
            _ => {
                return Err(self.throw("TypeError", "Class extends value is not a constructor or null"))
            }
        };
        let derived = parent.is_some();

        let proto = Object::new(proto_parent.clone());

        // The constructor: explicit member, or a synthesized default.
        let ctor_func = class
            .members
            .iter()
            .find(|m| m.kind == MemberKind::Constructor)
            .and_then(|m| m.func.clone())
            .unwrap_or_else(|| Rc::new(default_constructor(derived)));

        // Environments that carry the `super` bindings into methods/fields.
        let class_env = new_scope(Some(env.clone()));
        let inst_env = new_scope(Some(class_env.clone()));
        bind(&inst_env, "%superproto%", opt_obj(&proto_parent));
        bind(&inst_env, "%superclass%", ctor_parent.clone().unwrap_or(Value::Undefined));
        let static_env = new_scope(Some(class_env.clone()));
        bind(&static_env, "%superproto%", ctor_parent.clone().unwrap_or(Value::Undefined));

        // Build the constructor object on `proto`.
        let ctor_val = self.make_function(ctor_func, inst_env.clone());
        let ctor_obj = ctor_val.as_obj().unwrap().clone();
        {
            let mut b = ctor_obj.borrow_mut();
            b.props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
            b.proto = match &ctor_parent {
                Some(Value::Obj(p)) => Some(p.clone()),
                _ => Some(self.function_proto.clone()),
            };
            if let Some(n) = &class.name {
                b.props.insert("name", Property::data(Value::from_string(n.clone()), false, false, true));
            }
        }
        proto.borrow_mut().props.insert("constructor", Property::builtin(ctor_val.clone()));
        bind(&inst_env, "%thisctor%", ctor_val.clone());

        // Methods, accessors and fields.
        let mut inst_fields: Vec<(String, Option<Expr>)> = Vec::new();
        for m in &class.members {
            if m.kind == MemberKind::Constructor {
                continue;
            }
            let key = self.eval_prop_key(&m.key, env)?;
            let menv = if m.is_static { &static_env } else { &inst_env };
            let target = if m.is_static { ctor_obj.clone() } else { proto.clone() };
            match m.kind {
                MemberKind::Method => {
                    let f = self.make_function(m.func.clone().unwrap(), menv.clone());
                    if let Value::Obj(fo) = &f {
                        fo.borrow_mut()
                            .props
                            .insert("name", Property::data(Value::from_string(key.clone()), false, false, true));
                    }
                    target.borrow_mut().props.insert(key, Property::builtin(f));
                }
                MemberKind::Get | MemberKind::Set => {
                    let f = self.make_function(m.func.clone().unwrap(), menv.clone());
                    let (get, set) =
                        if m.kind == MemberKind::Get { (Some(f), None) } else { (None, Some(f)) };
                    self.define_class_accessor(&target, &key, get, set);
                }
                MemberKind::Field => {
                    if m.is_static {
                        let scope = new_scope(Some(static_env.clone()));
                        bind(&scope, "this", ctor_val.clone());
                        let v = match &m.value {
                            Some(e) => self.eval(e, &scope)?,
                            None => Value::Undefined,
                        };
                        ctor_obj.borrow_mut().props.insert(key, Property::plain(v));
                    } else {
                        inst_fields.push((key, m.value.clone()));
                    }
                }
                MemberKind::StaticBlock => {
                    let scope = new_scope(Some(static_env.clone()));
                    bind(&scope, "this", ctor_val.clone());
                    if let Some(func) = &m.func {
                        for stmt in &func.body {
                            self.exec_stmt(stmt, &scope)?;
                        }
                    }
                }
                MemberKind::Constructor => {}
            }
        }

        self.class_info.insert(
            Rc::as_ptr(&ctor_obj) as usize,
            ClassInfo { fields: inst_fields, field_env: inst_env, derived },
        );
        Ok(ctor_val)
    }

    fn define_class_accessor(&self, target: &Gc, key: &str, get: Option<Value>, set: Option<Value>) {
        let mut b = target.borrow_mut();
        if let Some(p) = b.props.get_mut(key) {
            if p.accessor {
                if get.is_some() {
                    p.get = get;
                }
                if set.is_some() {
                    p.set = set;
                }
                return;
            }
        }
        b.props.insert(
            key,
            Property {
                value: Value::Undefined,
                get,
                set,
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }

    /// Run a constructor's body against an already-allocated `this`, used by both `construct` and
    /// `super(...)`. Handles base-class field init, derived classes (their `super()` does the work),
    /// plain function constructors, and native parents (e.g. `extends Error`).
    pub fn run_constructor_on(
        &mut self,
        ctor: &Value,
        this: &Value,
        args: &[Value],
    ) -> Result<(), Abrupt> {
        let obj = match ctor {
            Value::Obj(o) => o.clone(),
            _ => return Err(self.throw("TypeError", "super target is not a constructor")),
        };
        let ptr = Rc::as_ptr(&obj) as usize;
        let is_class = self.class_info.contains_key(&ptr);
        let derived = self.class_info.get(&ptr).map(|i| i.derived).unwrap_or(false);
        let call = obj.borrow().call.clone();
        match call {
            Callable::User(func, cenv) => {
                // A base class initializes its fields before its body runs; a derived class does so
                // inside its own `super()`.
                if is_class && !derived {
                    self.init_instance_fields(ctor, this)?;
                }
                self.call_user(&func, cenv, this.clone(), args, true, &obj)?;
                Ok(())
            }
            Callable::Native(f) => {
                // Native parent (e.g. Error/Map): a super() call is a construct, so set the flag for
                // constructors that require `new`. Run it, then graft its own props onto `this`.
                let saved = self.constructing;
                self.constructing = true;
                let made = f(self, this.clone(), args).map_err(Abrupt::Throw);
                self.constructing = saved;
                let made = made?;
                if let (Value::Obj(src), Value::Obj(dst)) = (&made, this) {
                    if !Rc::ptr_eq(src, dst) {
                        for k in src.borrow().props.keys() {
                            let p = src.borrow().props.get(&k).cloned().unwrap();
                            dst.borrow_mut().props.insert(k, p);
                        }
                        // A subclass instance inherits the built-in's exotic behavior (e.g. an Array
                        // subclass is itself an Array exotic).
                        let src_exotic = src.borrow().exotic.clone();
                        if !matches!(src_exotic, crate::value::Exotic::None) {
                            dst.borrow_mut().exotic = src_exotic;
                        }
                        // Move the native object's internal slots (Map/Set/TypedArray/buffer/etc.)
                        // onto `this`, so a subclass instance carries the built-in's state.
                        let (sp, dp) = (Rc::as_ptr(src) as usize, Rc::as_ptr(dst) as usize);
                        if let Some(v) = self.map_data.remove(&sp) {
                            self.map_data.insert(dp, v);
                        }
                        if let Some(v) = self.typed_arrays.remove(&sp) {
                            self.typed_arrays.insert(dp, v);
                        }
                        if let Some(v) = self.data_views.remove(&sp) {
                            self.data_views.insert(dp, v);
                        }
                        if let Some(v) = self.array_buffers.remove(&sp) {
                            self.array_buffers.insert(dp, v);
                        }
                        if let Some(v) = self.regexps.remove(&sp) {
                            self.regexps.insert(dp, v);
                        }
                        if let Some(v) = self.promises.remove(&sp) {
                            self.promises.insert(dp, v);
                        }
                        if let Some(v) = self.temporal.remove(&sp) {
                            self.temporal.insert(dp, v);
                        }
                    }
                }
                Ok(())
            }
            _ => Err(self.throw("TypeError", "super target is not a constructor")),
        }
    }

    fn init_instance_fields(&mut self, ctor: &Value, this: &Value) -> Result<(), Abrupt> {
        let obj = match ctor {
            Value::Obj(o) => o.clone(),
            _ => return Ok(()),
        };
        let ptr = Rc::as_ptr(&obj) as usize;
        let (fields, field_env) = match self.class_info.get(&ptr) {
            Some(i) => (i.fields.clone(), i.field_env.clone()),
            None => return Ok(()),
        };
        for (key, init) in fields {
            let scope = new_scope(Some(field_env.clone()));
            bind(&scope, "this", this.clone());
            let v = match init {
                Some(e) => {
                    let v = self.eval(&e, &scope)?;
                    if is_anonymous_fn(&e) {
                        self.set_fn_name(&v, &key);
                    }
                    v
                }
                None => Value::Undefined,
            };
            self.set_member(this, &key, v)?;
        }
        Ok(())
    }

    fn eval_unary(&mut self, op: &str, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        if op == "typeof" {
            // typeof on an unresolved identifier yields "undefined" rather than throwing.
            if let Expr::Ident(name) = arg {
                match self.get_var(name, env) {
                    Ok(v) => return Ok(Value::from_string(v.type_of().to_string())),
                    // A binding in its temporal dead zone still throws; only a truly-unresolved
                    // name yields "undefined".
                    Err(e) if self.binding_in_tdz(name, env) => return Err(e),
                    Err(_) => return Ok(Value::str("undefined")),
                }
            }
            let v = self.eval(arg, env)?;
            return Ok(Value::from_string(v.type_of().to_string()));
        }
        if op == "delete" {
            return self.eval_delete(arg, env);
        }
        let v = self.eval(arg, env)?;
        if let Value::BigInt(n) = v {
            return match op {
                "!" => Ok(Value::Bool(n == 0)),
                "-" => Ok(Value::BigInt(n.wrapping_neg())),
                "~" => Ok(Value::BigInt(!n)),
                "void" => Ok(Value::Undefined),
                "+" => Err(self.throw("TypeError", "Cannot convert a BigInt value to a number")),
                _ => unreachable!("unary {op}"),
            };
        }
        match op {
            "!" => Ok(Value::Bool(!self.to_boolean(&v))),
            "-" => Ok(Value::Num(-self.to_number(&v)?)),
            "+" => Ok(Value::Num(self.to_number(&v)?)),
            "~" => Ok(Value::Num(!(self.to_int32(&v)?) as f64)),
            "void" => Ok(Value::Undefined),
            _ => unreachable!("unary {op}"),
        }
    }

    fn eval_delete(&mut self, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        match arg {
            Expr::Member { obj, prop, .. } => {
                let base = self.eval(obj, env)?;
                if let Value::Obj(o) = &base {
                    let ptr = Rc::as_ptr(o) as usize;
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        return Ok(Value::Bool(self.proxy_delete(target, handler, prop)?));
                    }
                    let configurable = o.borrow().props.get(prop).map(|p| p.configurable).unwrap_or(true);
                    if configurable {
                        o.borrow_mut().props.remove(prop);
                        return Ok(Value::Bool(true));
                    }
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(true))
            }
            Expr::Index { obj, index, .. } => {
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                if let Value::Obj(o) = &base {
                    let ptr = Rc::as_ptr(o) as usize;
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        return Ok(Value::Bool(self.proxy_delete(target, handler, &key)?));
                    }
                    let configurable = o.borrow().props.get(&key).map(|p| p.configurable).unwrap_or(true);
                    if configurable {
                        o.borrow_mut().props.remove(&key);
                        return Ok(Value::Bool(true));
                    }
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(true))
            }
            _ => Ok(Value::Bool(true)),
        }
    }

    /// Proxy `[[Delete]]`: call the deleteProperty trap, or forward to the target.
    pub(crate) fn proxy_delete(&mut self, target: Value, handler: Value, key: &str) -> Result<bool, Abrupt> {
        let trap = self.get_member(&handler, "deleteProperty")?;
        if trap.is_callable() {
            let kv = self.sym_from_key(key).unwrap_or_else(|| Value::from_string(key.to_string()));
            let res = self.call(trap, handler, &[target, kv])?;
            Ok(self.to_boolean(&res))
        } else if let Value::Obj(t) = &target {
            let configurable = t.borrow().props.get(key).map(|p| p.configurable).unwrap_or(true);
            if configurable {
                t.borrow_mut().props.remove(key);
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(true)
        }
    }

    fn eval_update(&mut self, op: &str, prefix: bool, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        let old = self.eval(arg, env)?;
        if let Value::BigInt(n) = old {
            let new = if op == "++" { n.wrapping_add(1) } else { n.wrapping_sub(1) };
            self.assign_to_target(arg, Value::BigInt(new), env)?;
            return Ok(Value::BigInt(if prefix { new } else { n }));
        }
        let n = self.to_number(&old)?;
        let new = if op == "++" { n + 1.0 } else { n - 1.0 };
        self.assign_to_target(arg, Value::Num(new), env)?;
        Ok(Value::Num(if prefix { new } else { n }))
    }

    fn eval_assign(&mut self, op: &str, target: &Expr, value: &Expr, env: &Env) -> Result<Value, Abrupt> {
        if op == "=" {
            let v = self.eval(value, env)?;
            // `f = function(){}` names the anonymous function after the target identifier.
            if let Expr::Ident(n) = target {
                if is_anonymous_fn(value) {
                    self.set_fn_name(&v, n);
                }
            }
            self.assign_to_target(target, v.clone(), env)?;
            return Ok(v);
        }
        // Logical assignment (&&=, ||=, ??=) short-circuits.
        if matches!(op, "&&=" | "||=" | "??=") {
            let cur = self.eval(target, env)?;
            let do_assign = match op {
                "&&=" => self.to_boolean(&cur),
                "||=" => !self.to_boolean(&cur),
                "??=" => matches!(cur, Value::Undefined | Value::Null),
                _ => unreachable!(),
            };
            if !do_assign {
                return Ok(cur);
            }
            let v = self.eval(value, env)?;
            self.assign_to_target(target, v.clone(), env)?;
            return Ok(v);
        }
        // Compound arithmetic/bitwise: a op= b  ≡  a = a <op> b.
        let cur = self.eval(target, env)?;
        let rhs = self.eval(value, env)?;
        let bin_op = &op[..op.len() - 1];
        let result = self.binary(bin_op, cur, rhs)?;
        self.assign_to_target(target, result.clone(), env)?;
        Ok(result)
    }

    fn assign_to_target(&mut self, target: &Expr, value: Value, env: &Env) -> Result<(), Abrupt> {
        match target {
            Expr::Ident(name) => self.assign_var(name, value, env),
            Expr::Member { obj, prop, .. } => {
                let base = self.eval(obj, env)?;
                self.set_member(&base, prop, value)
            }
            Expr::Index { obj, index, .. } => {
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                self.set_member(&base, &key, value)
            }
            // Destructuring assignment: an array/object literal reinterpreted as a target.
            Expr::Array(elems) => {
                let (iter, next) = self.get_iterator(&value)?;
                let iter_close = iter.clone();
                let mut done = false;
                let result = (|me: &mut Self| -> Result<(), Abrupt> {
                    for el in elems {
                        match el {
                            ArrayElem::Hole => {
                                if !done && me.iterator_step(&iter, &next)?.is_none() {
                                    done = true;
                                }
                            }
                            ArrayElem::Spread(t) => {
                                let mut rest = Vec::new();
                                while !done {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => rest.push(x),
                                        None => done = true,
                                    }
                                }
                                let arr = me.make_array(rest);
                                me.assign_to_target(t, arr, env)?;
                            }
                            ArrayElem::Item(t) => {
                                let v = if done {
                                    Value::Undefined
                                } else {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => x,
                                        None => {
                                            done = true;
                                            Value::Undefined
                                        }
                                    }
                                };
                                me.assign_destructure_elem(t, v, env)?;
                            }
                        }
                    }
                    Ok(())
                })(self);
                if !done {
                    self.iterator_close(&iter_close);
                }
                result
            }
            Expr::Object(props) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut taken: Vec<String> = Vec::new();
                for prop in props {
                    match prop {
                        PropDef::KeyValue { key, value: t } => {
                            let k = self.propkey_to_string(key, env)?;
                            let v = self.get_member(&value, &k)?;
                            taken.push(k);
                            self.assign_destructure_elem(t, v, env)?;
                        }
                        PropDef::Spread(t) => {
                            let rest = self.new_object();
                            if let Value::Obj(src) = &value {
                                let keys: Vec<Rc<str>> = src.borrow().props.keys();
                                for k in keys {
                                    if Interp::is_sym_key(&k) || taken.iter().any(|x| x.as_str() == &*k) {
                                        continue;
                                    }
                                    let enumerable =
                                        src.borrow().props.get(&k).map(|p| p.enumerable).unwrap_or(false);
                                    if enumerable {
                                        let v = self.get_member(&value, &k)?;
                                        rest.borrow_mut().props.insert(k, crate::value::Property::plain(v));
                                    }
                                }
                            }
                            self.assign_to_target(t, Value::Obj(rest), env)?;
                        }
                        _ => return Err(self.throw("SyntaxError", "invalid destructuring target")),
                    }
                }
                Ok(())
            }
            _ => Err(self.throw("ReferenceError", "invalid assignment target")),
        }
    }

    /// Assign one destructured element, honoring a `target = default` cover.
    fn assign_destructure_elem(&mut self, t: &Expr, v: Value, env: &Env) -> Result<(), Abrupt> {
        if let Expr::Assign { op: "=", target, value: dflt } = t {
            let v = if matches!(v, Value::Undefined) {
                let dv = self.eval(dflt, env)?;
                if let (Expr::Ident(n), true) = (&**target, is_anonymous_fn(dflt)) {
                    self.set_fn_name(&dv, n);
                }
                dv
            } else {
                v
            };
            self.assign_to_target(target, v, env)
        } else {
            self.assign_to_target(t, v, env)
        }
    }

    fn propkey_to_string(&mut self, key: &PropKey, env: &Env) -> Result<String, Abrupt> {
        Ok(match key {
            PropKey::Ident(s) => s.clone(),
            PropKey::Str(s) => s.to_string(),
            PropKey::Num(n) => self.num_to_str(*n),
            PropKey::Computed(e) => {
                let kv = self.eval(e, env)?;
                self.to_property_key(&kv)?
            }
        })
    }

    // ----- operators --------------------------------------------------------------------------

    fn binary(&mut self, op: &str, l: Value, r: Value) -> Result<Value, Abrupt> {
        // Arithmetic, bitwise, and `+` convert both operands to primitives first (left then right),
        // then dispatch on BigInt / string (for `+`) / number. ToPrimitive runs before the BigInt
        // mixing check so a wrapped BigInt object coerces correctly.
        if matches!(op, "+" | "-" | "*" | "/" | "%" | "**" | "&" | "|" | "^" | "<<" | ">>") {
            let hint = if op == "+" { Hint::Default } else { Hint::Number };
            let lp = self.to_primitive(&l, hint)?;
            let rp = self.to_primitive(&r, hint)?;
            if op == "+" && (matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_))) {
                let ls = self.to_string(&lp)?;
                let rs = self.to_string(&rp)?;
                if ls.len() + rs.len() > MAX_STR_LEN {
                    return Err(self.throw("RangeError", "Invalid string length"));
                }
                return Ok(Value::from_string(format!("{ls}{rs}")));
            }
            if matches!(lp, Value::BigInt(_)) || matches!(rp, Value::BigInt(_)) {
                if let (Value::BigInt(x), Value::BigInt(y)) = (&lp, &rp) {
                    return self.bigint_binop(op, *x, *y);
                }
                return Err(self.throw(
                    "TypeError",
                    "Cannot mix BigInt and other types, use explicit conversions",
                ));
            }
            return match op {
                "+" => Ok(Value::Num(self.to_number(&lp)? + self.to_number(&rp)?)),
                "-" => Ok(Value::Num(self.to_number(&lp)? - self.to_number(&rp)?)),
                "*" => Ok(Value::Num(self.to_number(&lp)? * self.to_number(&rp)?)),
                "/" => Ok(Value::Num(self.to_number(&lp)? / self.to_number(&rp)?)),
                "%" => {
                    let a = self.to_number(&lp)?;
                    let b = self.to_number(&rp)?;
                    Ok(Value::Num(js_mod(a, b)))
                }
                "**" => Ok(Value::Num(self.to_number(&lp)?.powf(self.to_number(&rp)?))),
                "&" => Ok(Value::Num((self.to_int32(&lp)? & self.to_int32(&rp)?) as f64)),
                "|" => Ok(Value::Num((self.to_int32(&lp)? | self.to_int32(&rp)?) as f64)),
                "^" => Ok(Value::Num((self.to_int32(&lp)? ^ self.to_int32(&rp)?) as f64)),
                "<<" => {
                    let a = self.to_int32(&lp)?;
                    let b = (self.to_uint32(&rp)?) & 31;
                    Ok(Value::Num((a.wrapping_shl(b)) as f64))
                }
                ">>" => {
                    let a = self.to_int32(&lp)?;
                    let b = (self.to_uint32(&rp)?) & 31;
                    Ok(Value::Num((a >> b) as f64))
                }
                _ => unreachable!(),
            };
        }
        match op {
            "==" => Ok(Value::Bool(self.loose_equals(&l, &r)?)),
            "!=" => Ok(Value::Bool(!self.loose_equals(&l, &r)?)),
            "===" => Ok(Value::Bool(self.strict_equals(&l, &r))),
            "!==" => Ok(Value::Bool(!self.strict_equals(&l, &r))),
            "<" | ">" | "<=" | ">=" => self.compare(op, l, r),
            "&" => Ok(Value::Num((self.to_int32(&l)? & self.to_int32(&r)?) as f64)),
            "|" => Ok(Value::Num((self.to_int32(&l)? | self.to_int32(&r)?) as f64)),
            "^" => Ok(Value::Num((self.to_int32(&l)? ^ self.to_int32(&r)?) as f64)),
            "<<" => {
                let a = self.to_int32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a.wrapping_shl(b)) as f64))
            }
            ">>" => {
                let a = self.to_int32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a >> b) as f64))
            }
            ">>>" => {
                let a = self.to_uint32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a >> b) as f64))
            }
            "instanceof" => self.instanceof(&l, &r),
            "in" => {
                if let Value::Obj(o) = &r {
                    let key = self.to_property_key(&l)?;
                    let ptr = Rc::as_ptr(o) as usize;
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        let trap = self.get_member(&handler, "has")?;
                        if trap.is_callable() {
                            let res = self.call(trap, handler, &[target, Value::str(key.as_str())])?;
                            return Ok(Value::Bool(self.to_boolean(&res)));
                        }
                        if let Value::Obj(to) = &target {
                            return Ok(Value::Bool(self.has_property(to, &key)));
                        }
                    }
                    Ok(Value::Bool(self.has_property(o, &key)))
                } else {
                    Err(self.throw("TypeError", "'in' requires an object on the right"))
                }
            }
            _ => unreachable!("binary {op}"),
        }
    }

    fn bigint_binop(&self, op: &str, x: i128, y: i128) -> Result<Value, Abrupt> {
        let v = match op {
            "+" => x.wrapping_add(y),
            "-" => x.wrapping_sub(y),
            "*" => x.wrapping_mul(y),
            "/" => {
                if y == 0 {
                    return Err(self.throw("RangeError", "Division by zero"));
                }
                x.wrapping_div(y)
            }
            "%" => {
                if y == 0 {
                    return Err(self.throw("RangeError", "Division by zero"));
                }
                x.wrapping_rem(y)
            }
            "**" => {
                if y < 0 {
                    return Err(self.throw("RangeError", "Exponent must be non-negative"));
                }
                x.wrapping_pow(y.min(u32::MAX as i128) as u32)
            }
            "&" => x & y,
            "|" => x | y,
            "^" => x ^ y,
            "<<" => x.wrapping_shl(y.clamp(0, 127) as u32),
            ">>" => x.wrapping_shr(y.clamp(0, 127) as u32),
            _ => return Err(self.throw("TypeError", "unsupported BigInt operator")),
        };
        Ok(Value::BigInt(v))
    }

    /// ToBigInt: BigInt stays; Boolean → 0n/1n; String parses (SyntaxError if malformed); Number /
    /// undefined / null / Symbol throw TypeError.
    pub(crate) fn to_bigint(&mut self, v: &Value) -> Result<i128, Abrupt> {
        let p = self.to_primitive(v, Hint::Number)?;
        match p {
            Value::BigInt(n) => Ok(n),
            Value::Bool(b) => Ok(b as i128),
            Value::Str(s) => {
                let t = s.trim();
                let parsed = if t.is_empty() {
                    Some(0)
                } else if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    i128::from_str_radix(h, 16).ok()
                } else if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                    i128::from_str_radix(o, 8).ok()
                } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                    i128::from_str_radix(b, 2).ok()
                } else {
                    t.parse::<i128>().ok()
                };
                parsed.ok_or_else(|| self.throw("SyntaxError", "Cannot convert string to a BigInt"))
            }
            Value::Num(_) => Err(self.throw("TypeError", "Cannot convert a Number to a BigInt")),
            _ => Err(self.throw("TypeError", "Cannot convert value to a BigInt")),
        }
    }

    fn compare(&mut self, op: &str, l: Value, r: Value) -> Result<Value, Abrupt> {
        let lp = self.to_primitive(&l, Hint::Number)?;
        let rp = self.to_primitive(&r, Hint::Number)?;
        if let (Value::Str(a), Value::Str(b)) = (&lp, &rp) {
            let res = match op {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                ">=" => a >= b,
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        // Comparisons accept mixed BigInt/Number operands (compared as real numbers).
        let bigint_f64 = |v: &Value| -> Option<f64> {
            match v {
                Value::BigInt(n) => Some(*n as f64),
                _ => None,
            }
        };
        let a = match bigint_f64(&lp) {
            Some(n) => n,
            None => self.to_number(&lp)?,
        };
        let b = match bigint_f64(&rp) {
            Some(n) => n,
            None => self.to_number(&rp)?,
        };
        if a.is_nan() || b.is_nan() {
            return Ok(Value::Bool(false));
        }
        let res = match op {
            "<" => a < b,
            ">" => a > b,
            "<=" => a <= b,
            ">=" => a >= b,
            _ => unreachable!(),
        };
        Ok(Value::Bool(res))
    }

    fn instanceof(&mut self, l: &Value, r: &Value) -> Result<Value, Abrupt> {
        let ctor = match r {
            Value::Obj(o) if !matches!(o.borrow().call, Callable::None) => o.clone(),
            _ => return Err(self.throw("TypeError", "right-hand side of instanceof is not callable")),
        };
        let proto = match ctor.borrow().props.get("prototype").map(|p| p.value.clone()) {
            Some(Value::Obj(p)) => p,
            _ => return Ok(Value::Bool(false)),
        };
        let mut cur = match l {
            Value::Obj(o) => o.borrow().proto.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        while let Some(o) = cur {
            if Rc::ptr_eq(&o, &proto) {
                return Ok(Value::Bool(true));
            }
            cur = o.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    }

    // ----- abstract operations ----------------------------------------------------------------

    pub fn to_boolean(&self, v: &Value) -> bool {
        match v {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::BigInt(n) => *n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Sym(_) | Value::Obj(_) => true,
        }
    }

    pub fn to_number(&mut self, v: &Value) -> Result<f64, Abrupt> {
        Ok(match v {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Num(n) => *n,
            Value::BigInt(_) => {
                return Err(self.throw("TypeError", "Cannot convert a BigInt value to a number"))
            }
            Value::Str(s) => parse_number(s),
            Value::Sym(_) => {
                return Err(self.throw("TypeError", "Cannot convert a Symbol value to a number"))
            }
            Value::Obj(_) => {
                let p = self.to_primitive(v, Hint::Number)?;
                self.to_number(&p)?
            }
        })
    }

    pub fn to_int32(&mut self, v: &Value) -> Result<i32, Abrupt> {
        let n = self.to_number(v)?;
        Ok(to_int32(n))
    }
    pub fn to_uint32(&mut self, v: &Value) -> Result<u32, Abrupt> {
        let n = self.to_number(v)?;
        Ok(to_int32(n) as u32)
    }

    pub fn to_string(&mut self, v: &Value) -> Result<Rc<str>, Abrupt> {
        Ok(match v {
            Value::Undefined => Rc::from("undefined"),
            Value::Null => Rc::from("null"),
            Value::Bool(b) => Rc::from(if *b { "true" } else { "false" }),
            Value::Num(n) => Rc::from(self.num_to_str(*n).as_str()),
            Value::BigInt(n) => Rc::from(n.to_string().as_str()),
            Value::Str(s) => s.clone(),
            Value::Sym(_) => {
                return Err(self.throw("TypeError", "Cannot convert a Symbol value to a string"))
            }
            Value::Obj(_) => {
                let p = self.to_primitive(v, Hint::String)?;
                match p {
                    Value::Obj(_) => return Err(self.throw("TypeError", "cannot convert object to string")),
                    other => self.to_string(&other)?,
                }
            }
        })
    }

    pub fn to_property_key(&mut self, v: &Value) -> Result<String, Abrupt> {
        // A symbol key maps to its internal NUL-prefixed key; everything else is its string form.
        if let Value::Sym(s) = v {
            return Ok(Interp::sym_key(s));
        }
        Ok(self.to_string(v)?.to_string())
    }

    /// The internal property key for a well-known symbol (e.g. `Symbol.toPrimitive`).
    fn well_known_sym_key(&self, name: &str) -> Option<String> {
        let sym = self.global.borrow().props.get("Symbol").map(|p| p.value.clone())?;
        if let Value::Obj(o) = sym {
            if let Some(p) = o.borrow().props.get(name) {
                if let Value::Sym(d) = &p.value {
                    return Some(Interp::sym_key(d));
                }
            }
        }
        None
    }

    pub fn to_primitive(&mut self, v: &Value, hint: Hint) -> Result<Value, Abrupt> {
        let obj = match v {
            Value::Obj(o) => o.clone(),
            _ => return Ok(v.clone()),
        };
        // A `@@toPrimitive` method takes precedence over valueOf/toString.
        if let Some(key) = self.well_known_sym_key("toPrimitive") {
            let f = self.get_member(&Value::Obj(obj.clone()), &key)?;
            if f.is_callable() {
                let hint_str = match hint {
                    Hint::String => "string",
                    Hint::Number => "number",
                    Hint::Default => "default",
                };
                let r = self.call(f, v.clone(), &[Value::str(hint_str)])?;
                if matches!(r, Value::Obj(_)) {
                    return Err(self.throw("TypeError", "Cannot convert object to a primitive value"));
                }
                return Ok(r);
            }
        }
        let order: [&str; 2] = match hint {
            Hint::String => ["toString", "valueOf"],
            _ => ["valueOf", "toString"],
        };
        for method in order {
            let f = self.get_member(&Value::Obj(obj.clone()), method)?;
            if f.is_callable() {
                let r = self.call(f, v.clone(), &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Err(self.throw("TypeError", "cannot convert object to primitive value"))
    }

    pub fn num_to_str(&self, n: f64) -> String {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() };
        }
        if n == 0.0 {
            return "0".to_string();
        }
        format!("{n}")
    }

    pub fn strict_equals(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Undefined, Value::Undefined) => true,
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Sym(x), Value::Sym(y)) => x.id == y.id,
            (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    pub fn loose_equals(&mut self, a: &Value, b: &Value) -> Result<bool, Abrupt> {
        Ok(match (a, b) {
            (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::BigInt(x), Value::Num(y)) | (Value::Num(y), Value::BigInt(x)) => {
                y.is_finite() && y.fract() == 0.0 && (*x as f64) == *y
            }
            (Value::BigInt(x), Value::Str(s)) | (Value::Str(s), Value::BigInt(x)) => {
                s.trim().parse::<i128>().map(|n| n == *x).unwrap_or(false)
            }
            (Value::BigInt(_), Value::Obj(_)) => {
                let bp = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, &bp)?
            }
            (Value::Obj(_), Value::BigInt(_)) => {
                let ap = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&ap, b)?
            }
            (Value::Num(_), Value::Num(_))
            | (Value::Str(_), Value::Str(_))
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Sym(_), Value::Sym(_))
            | (Value::Obj(_), Value::Obj(_)) => self.strict_equals(a, b),
            (Value::Num(_), Value::Str(_)) => {
                let bn = self.to_number(b)?;
                self.strict_equals(a, &Value::Num(bn))
            }
            (Value::Str(_), Value::Num(_)) => {
                let an = self.to_number(a)?;
                self.strict_equals(&Value::Num(an), b)
            }
            (Value::Bool(_), _) => {
                let an = self.to_number(a)?;
                self.loose_equals(&Value::Num(an), b)?
            }
            (_, Value::Bool(_)) => {
                let bn = self.to_number(b)?;
                self.loose_equals(a, &Value::Num(bn))?
            }
            (Value::Obj(_), Value::Num(_) | Value::Str(_)) => {
                let ap = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&ap, b)?
            }
            (Value::Num(_) | Value::Str(_), Value::Obj(_)) => {
                let bp = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, &bp)?
            }
            _ => false,
        })
    }
}

#[derive(Clone, Copy)]
pub enum Hint {
    Default,
    Number,
    String,
}

enum LoopStep {
    Continue,
    Done,
}

/// Insert an initialized, mutable binding into `env` (used for the hidden `%super*%`/`this` slots).
fn bind(env: &Env, name: &str, value: Value) {
    env.borrow_mut()
        .vars
        .insert(name.to_string(), Binding { value, mutable: true, initialized: true, import_ref: None });
}

fn opt_obj(o: &Option<Gc>) -> Value {
    match o {
        Some(g) => Value::Obj(g.clone()),
        None => Value::Undefined,
    }
}

/// The synthesized default class constructor. Derived: `constructor(...args) { super(...args); }`;
/// base: `constructor() {}`.
fn default_constructor(derived: bool) -> Function {
    let body = if derived {
        vec![Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Super),
            args: vec![ArrayElem::Spread(Expr::Ident("args".to_string()))],
            optional: false,
        })]
    } else {
        Vec::new()
    };
    let params = if derived {
        vec![Param { pattern: Pattern::Ident("args".to_string()), default: None, rest: true }]
    } else {
        Vec::new()
    };
    Function {
        name: None,
        params,
        body,
        is_arrow: false,
        is_strict: true,
        expr_body: false,
        is_generator: false,
        is_async: false,
    }
}

fn promise_resolve_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    i.resolve_promise(&this, args.first().cloned().unwrap_or(Value::Undefined));
    Ok(Value::Undefined)
}

fn promise_reject_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    i.reject_promise(&this, args.first().cloned().unwrap_or(Value::Undefined));
    Ok(Value::Undefined)
}

fn describe_callee(callee: &Expr) -> String {
    match callee {
        Expr::Ident(n) => n.clone(),
        Expr::Member { prop, .. } => format!("(intermediate value).{prop}"),
        _ => "expression".to_string(),
    }
}

/// Whether an expression is an anonymous function/arrow/class (eligible for NamedEvaluation).
pub(crate) fn is_anonymous_fn(e: &Expr) -> bool {
    match e {
        Expr::Func(f) => f.name.is_none(),
        Expr::Class(c) => c.name.is_none(),
        _ => false,
    }
}

fn js_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        return f64::NAN;
    }
    if b.is_infinite() {
        return a;
    }
    if a == 0.0 {
        return a;
    }
    a % b
}

fn to_int32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc();
    let m = n.rem_euclid(4294967296.0);
    if m >= 2147483648.0 {
        (m - 4294967296.0) as i32
    } else {
        m as i32
    }
}

/// Parse a string to a Number per the (simplified) StringToNumber grammar: trimmed, empty → 0,
/// supports decimals, `Infinity`, and `0x`/`0o`/`0b` radix prefixes.
fn parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map(|n| sign * n as f64).unwrap_or(f64::NAN);
    }
    if let Some(oct) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8).map(|n| sign * n as f64).unwrap_or(f64::NAN);
    }
    if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2).map(|n| sign * n as f64).unwrap_or(f64::NAN);
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}
