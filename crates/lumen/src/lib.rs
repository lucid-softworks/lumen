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
mod builtins;
mod coroutine;
mod eval;
mod interpreter;
mod intl;
mod lexer;
mod modules;
mod numbering;
mod parser;
mod regex;
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
mod unicode_props;
mod value;

use interpreter::Interp;
use value::Value;

/// A parse-phase failure. test262 reports these as a `SyntaxError` thrown during parsing.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub line: u32,
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
        Engine {
            interp: Interp::new(),
        }
    }

    /// Run `src` as a spawned `$262.agent`: the agent may block in `Atomics.wait`, receives
    /// SharedArrayBuffer broadcasts on `broadcast_rx`, and reports back via `report_tx`.
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
            Ok(Value::Undefined) | Err(_) => String::new(),
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

#[cfg(test)]
mod tests;
