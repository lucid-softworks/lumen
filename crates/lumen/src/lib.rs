//! lumen — a from-scratch JavaScript engine (std-only, no dependencies).
//!
//! lumen is the eventual in-house replacement for the V8 backend in the `js` crate. Today it is a
//! tree-walking interpreter covering the ECMAScript language core, driven by the tc39/test262
//! conformance suite (see `crates/test262-runner`). It deliberately implements a growing *subset* —
//! the test262 score is the roadmap.
//!
//! ## Shape
//! - [`lexer`] tokenizes, [`parser`] builds the [`ast`], [`interpreter`] + `eval` walk it.
//! - [`value`] is the prototype-based object model (`Rc<RefCell<Object>>`, reference-counted — no
//!   real GC yet, so reference cycles leak; fine for the per-test runner).
//! - [`builtins`] installs the realm (`globalThis`, `Object`/`Array`/`Function`/`Math`, the error
//!   constructors, global functions).
//!
//! ## Public API
//! [`Engine::new`] builds a fresh realm; [`Engine::eval`] runs a script and reports a [`Completion`]
//! (a value, or a thrown error with its constructor name + message) or a parse-phase [`ParseError`].
//! The error name + phase distinction is exactly what a test262 negative-test matcher needs.

// The ECMAScript abstract operations (`to_number`/`to_string`/`to_primitive`/…) take `&mut self`
// on purpose: converting an object can run user `valueOf`/`toString`/getters, which mutate the
// realm. That trips clippy's `wrong_self_convention`, which assumes `to_*` is a cheap borrow.
#![allow(clippy::wrong_self_convention)]

mod ast;
mod bigint;
mod builtins;
pub mod bytecode;
mod coroutine;
mod eval;
mod fasthash;
mod host;
mod interpreter;
mod intl;
mod jstr;
mod lexer;
mod modules;
mod numbering;
mod parser;
mod regex;
mod regex_emoji;
mod regex_fold;
mod temporal;
mod token;
mod tz;
#[rustfmt::skip]
mod tzdata;
#[rustfmt::skip]
mod umalqura;
#[rustfmt::skip]
mod cldr_likely;
#[rustfmt::skip]
mod cldr_dates;
#[rustfmt::skip]
mod cldr_units;
#[rustfmt::skip]
mod units;
mod unicode_norm;
mod unicode_norm_impl;
mod unicode_props;
mod value;

use interpreter::Interp;
use value::Value;

/// Host wall-clock override: milliseconds since the Unix epoch. Targets without a usable
/// `SystemTime` (wasm32-unknown-unknown) install one at startup; when unset, `Date`/`Temporal.Now`
/// fall back to `SystemTime`.
static HOST_CLOCK: std::sync::OnceLock<fn() -> f64> = std::sync::OnceLock::new();

/// Install a process-wide wall-clock source (first call wins). The embedder's `f` returns
/// milliseconds since the Unix epoch.
pub fn set_host_clock(f: fn() -> f64) {
    let _ = HOST_CLOCK.set(f);
}

/// The installed host clock's current time, if one was set.
pub(crate) fn host_now_ms() -> Option<f64> {
    HOST_CLOCK.get().map(|f| f())
}

/// A parse-phase failure. test262 reports these as a `SyntaxError` thrown during parsing.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub line: u32,
    /// The parse failed only because the input ended too soon (e.g. an unclosed block or
    /// template). A REPL treats this as "keep reading lines", not a SyntaxError.
    pub at_eof: bool,
}

/// The outcome of evaluating a script.
pub enum Completion {
    /// Ran to completion; the last statement value rendered to a string (best-effort).
    Value(String),
    /// A value was thrown. `name` is the error's constructor name (`"TypeError"`, …) when the
    /// thrown value is an Error object, else `""`.
    Throw { name: String, message: String },
}

/// A JavaScript engine instance: one realm (global object + intrinsics) that persists across
/// [`eval`](Engine::eval) calls.
pub struct Engine {
    interp: Interp,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        interpreter::sym_for_reset();
        Engine {
            interp: Interp::new(),
        }
    }

    /// Run `src` as a spawned `$262.agent`: the agent may block in `Atomics.wait`, receives
    /// SharedArrayBuffer broadcasts on `broadcast_rx`, and reports back via `report_tx`.
    /// Whether this agent may block in `Atomics.wait` (test262's CanBlockIsTrue flag).
    pub fn set_can_block(&mut self, b: bool) {
        self.interp.can_block = b;
    }

    pub fn run_as_agent(
        &mut self,
        src: &str,
        broadcast_rx: std::sync::mpsc::Receiver<(u64, usize)>,
        report_tx: std::sync::mpsc::Sender<String>,
    ) {
        self.interp.can_block = true;
        self.interp.agent = Some(Box::new(interpreter::AgentChannels {
            agent_broadcast_txs: Vec::new(),
            report_rx: None,
            report_tx,
            broadcast_rx: Some(broadcast_rx),
        }));
        let _ = self.eval(src, false);
    }

    /// Parse and run `src`. `strict` forces strict mode (used for the test262 strict variant); a
    /// `"use strict"` directive in the source also enables it.
    pub fn eval(&mut self, src: &str, strict: bool) -> Result<Completion, ParseError> {
        let body = parser::parse_script(src, strict).map_err(|e| ParseError {
            message: e.message,
            line: e.line,
            at_eof: e.at_eof,
        })?;
        // A top-level `"use strict"` directive prologue turns on strict mode for the whole script.
        let directive_strict = matches!(
            body.first(),
            Some(ast::Stmt::Expr(ast::Expr::Str(s))) if &**s == "use strict"
        );
        self.interp.strict = strict || directive_strict;
        let result = self.interp.run_program(&body);
        // Run queued promise reactions (the microtask checkpoint after the script).
        self.interp.run_agent_event_loop();
        match result {
            Ok(v) => Ok(Completion::Value(self.render(&v))),
            Err(thrown) => Ok(self.describe_throw(thrown)),
        }
    }

    /// Install a host module loader used by dynamic `import()` (and `eval_module`). `loader(specifier,
    /// referrer)` returns the imported module's `(canonical_key, source)`.
    pub fn set_module_loader(
        &mut self,
        loader: impl Fn(&str, &str) -> Option<(String, String)> + 'static,
    ) {
        self.interp.module_loader = Some(std::rc::Rc::new(loader));
    }

    /// The default referrer for a bare `import()` in script code (so relative specifiers resolve).
    pub fn set_import_base(&mut self, base: &str) {
        self.interp.import_base = base.to_string();
    }

    /// Evaluate `src` as an ES module identified by `key`. `loader(specifier, referrer)` resolves an
    /// imported specifier to its `(canonical_key, source)`; it is consulted for every dependency.
    pub fn eval_module(
        &mut self,
        src: &str,
        key: &str,
        loader: impl Fn(&str, &str) -> Option<(String, String)> + 'static,
    ) -> Result<Completion, ParseError> {
        self.interp.module_loader = Some(std::rc::Rc::new(loader));
        let result = self.interp.load_module(key, src);
        self.interp.run_agent_event_loop();
        Ok(match result {
            Ok(_) => Completion::Value(String::new()),
            Err(a) => self.describe_throw(interpreter::abrupt_value(a)),
        })
    }

    /// Select the execution tier (see [`bytecode::Tier`]). `Interp` — the default — never
    /// touches any codegen path; `Bytecode` compiles eligible functions after
    /// [`set_tier_threshold`](Engine::set_tier_threshold) calls.
    pub fn set_tier(&mut self, tier: bytecode::Tier) {
        self.interp.tier = tier;
    }

    /// Calls before a function is considered for bytecode compilation (0 = immediately).
    pub fn set_tier_threshold(&mut self, threshold: u32) {
        self.interp.tier_threshold = threshold;
    }

    /// Drain anything written to `console.*` since the last call.
    pub fn take_console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.interp.console)
    }

    fn render(&mut self, v: &Value) -> String {
        self.interp
            .to_string(v)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn describe_throw(&mut self, thrown: Value) -> Completion {
        // Pull the constructor name + message off an Error object; fall back to the rendered value.
        let name = match self.interp.get_member(&thrown, "name") {
            Ok(Value::Undefined) | Err(_) => {
                // No own/inherited `name` (e.g. Test262Error): use the constructor's name.
                match self.interp.get_member(&thrown, "constructor") {
                    Ok(ctor @ Value::Obj(_)) => match self.interp.get_member(&ctor, "name") {
                        Ok(Value::Undefined) | Err(_) => String::new(),
                        Ok(v) => self.render(&v),
                    },
                    _ => String::new(),
                }
            }
            Ok(v) => self.render(&v),
        };
        let message = match &thrown {
            Value::Obj(_) => match self.interp.get_member(&thrown, "message") {
                Ok(Value::Undefined) | Err(_) => String::new(),
                Ok(v) => self.render(&v),
            },
            other => self.render(other),
        };
        Completion::Throw { name, message }
    }
}

/// The curated embedder surface (`feature = "embed"`), for runtime layers (event loop, host
/// APIs) built on top of the engine. Gated because everything here is a semver commitment on a
/// published crate; it stabilizes together with the `lumen-host`/`lumen-runtime` crates.
#[cfg(feature = "embed")]
pub mod embed {
    pub use crate::host::{OpState, ResourceId, ResourceTable};
    /// The context a [`NativeFn`] receives: a curated view of the interpreter. Only the
    /// audited embedder-safe methods are `pub`; the rest of the interpreter is `pub(crate)`.
    pub use crate::interpreter::Interp as Ctx;
    /// JS values. Matching/constructing the primitive variants is supported API; object
    /// internals stay opaque — an object handle is only usable through [`Ctx`] methods.
    pub use crate::value::{NativeFn, Value};
}

/// Embedder methods (`feature = "embed"`). Native functions registered here are bare `fn`
/// pointers (they cannot capture); Rust state lives in [`embed::OpState`], reached through the
/// `&mut Ctx` argument.
#[cfg(feature = "embed")]
impl Engine {
    /// Direct access to the native-function context (also where [`embed::OpState`] lives, via
    /// [`embed::Ctx::op_state`]).
    pub fn ctx(&mut self) -> &mut embed::Ctx {
        &mut self.interp
    }

    /// The realm's global object — the root from which an embedder reaches user-defined JS
    /// (e.g. `ctx().get_member(&engine.global_this(), "myCallback")`).
    pub fn global_this(&self) -> embed::Value {
        Value::Obj(self.interp.global.clone())
    }

    /// Define `globalThis.<name>` as a native function (non-enumerable, like built-ins).
    pub fn define_global(&mut self, name: &str, len: usize, f: embed::NativeFn) {
        let global = self.interp.global.clone();
        self.interp.def_method(&global, name, len, f);
    }

    /// Define `globalThis.<name>` as a namespace object (like `Math`) with the given
    /// `(name, arity, fn)` native methods.
    pub fn define_namespace(&mut self, name: &str, ops: &[(&str, usize, embed::NativeFn)]) {
        let ns = self.interp.new_object();
        for (op, len, f) in ops {
            self.interp.def_method(&ns, op, *len, *f);
        }
        self.interp
            .global
            .borrow_mut()
            .props
            .insert(name, crate::value::Property::builtin(Value::Obj(ns)));
    }

    /// Call a JS function value; `Err` is the thrown value. This is how the runtime's event
    /// loop re-enters the engine to fire a timer/IO callback, so it must work on every
    /// execution tier, not just the interpreter.
    pub fn call_function(
        &mut self,
        func: &embed::Value,
        this: embed::Value,
        args: &[embed::Value],
    ) -> Result<embed::Value, embed::Value> {
        self.interp
            .call(func.clone(), this, args)
            .map_err(interpreter::abrupt_value)
    }

    /// Drain the microtask (promise-reaction) queue to quiescence.
    pub fn run_microtasks(&mut self) {
        self.interp.drain_microtasks();
    }

    /// Whether promise-reaction jobs are queued (the loop uses this to decide when a turn is
    /// really over).
    pub fn has_pending_jobs(&self) -> bool {
        !self.interp.microtasks.is_empty()
    }

    /// Run a single queued job; `false` when the queue was empty.
    pub fn run_one_job(&mut self) -> bool {
        match self.interp.microtasks.pop_front() {
            Some(job) => {
                self.interp.run_job(job);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
