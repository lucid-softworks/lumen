//! Recursive-descent parser with Pratt precedence for binary operators. Produces the [`crate::ast`]
//! tree. Any failure is a [`ParseError`], which the engine reports as a SyntaxError (parse phase).

use crate::ast::*;
use crate::lexer::tokenize;
use crate::token::{Tok, Token, TplPart, KEYWORDS};
use std::rc::Rc;

pub struct ParseError {
    pub message: String,
    pub line: u32,
}

/// Parse a complete script. `strict` seeds strict mode (e.g. for the strict test262 variant); a
/// `"use strict"` directive prologue also turns it on.
pub fn parse_script(src: &str, strict: bool) -> Result<Vec<Stmt>, ParseError> {
    parse_script_eval(src, strict, false, false)
}

/// Parse eval code. Like [`parse_script`], but `allow_new_target` permits a top-level `new.target`
/// (a direct eval whose caller is inside a function).
pub fn parse_script_eval(
    src: &str,
    strict: bool,
    allow_new_target: bool,
    allow_super: bool,
) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(src).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        strict,
        depth: 0,
        in_generator: false,
        in_async: false,
        in_params: false,
        no_in: false,
        module: false,
        fn_depth: 0,
        iter_depth: 0,
        switch_depth: 0,
        labels: Vec::new(),
        decl_scopes: vec![DeclScope {
            fn_boundary: true,
            ..Default::default()
        }],
        next_scope_is_fn_boundary: false,
        allow_new_target,
        top_level: false,
        super_prop_ok: allow_super,
        proto_dups: Vec::new(),
        last_paren: false,
        single_stmt: false,
    };
    let strict_prologue = p.has_use_strict_prologue();
    p.strict = p.strict || strict_prologue;
    let body = p.parse_stmts_until_eof()?;
    validate_private_names(&body).map_err(|message| ParseError { message, line: 0 })?;
    Ok(body)
}

/// Parse a module (always strict; `import`/`export` are allowed only here). Modules permit top-level
/// `await`, so `await` is treated as a keyword at the module's top level.
pub fn parse_module(src: &str) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(src).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        strict: true,
        depth: 0,
        in_generator: false,
        in_async: true,
        in_params: false,
        no_in: false,
        module: true,
        fn_depth: 0,
        iter_depth: 0,
        switch_depth: 0,
        labels: Vec::new(),
        decl_scopes: vec![DeclScope {
            fn_boundary: true,
            ..Default::default()
        }],
        next_scope_is_fn_boundary: false,
        allow_new_target: false,
        top_level: false,
        super_prop_ok: false,
        proto_dups: Vec::new(),
        last_paren: false,
        single_stmt: false,
    };
    let body = p.parse_stmts_until_eof()?;
    validate_module(&body)?;
    validate_private_names(&body).map_err(|message| ParseError { message, line: 0 })?;
    Ok(body)
}

/// Module-level early errors: ExportedNames must be unique, top-level lexical+import bindings must be
/// unique (and disjoint from var names), and every `export { local }` must name a declared binding.
fn validate_module(body: &[Stmt]) -> Result<(), ParseError> {
    let mut exported: Vec<String> = Vec::new(); // ExportedNames
    let mut lexical: Vec<String> = Vec::new(); // let/const/class/function + imports
    let mut vars: Vec<String> = Vec::new();
    let mut export_locals: Vec<String> = Vec::new(); // `export { local }` (no source) must be bound

    for stmt in body {
        match stmt {
            Stmt::Import(decl) => {
                for spec in &decl.specs {
                    let name = match spec {
                        ImportSpec::Default(n) | ImportSpec::Namespace(n) => n,
                        ImportSpec::Named { local, .. } => local,
                    };
                    lexical.push(name.clone());
                }
            }
            Stmt::ExportDefault(inner) => {
                exported.push("default".to_string());
                // `export default function f(){}` / `class C{}` also binds the name lexically.
                if matches!(&**inner, Stmt::FuncDecl(_) | Stmt::ClassDecl(_)) {
                    collect_top_decl(inner, &mut lexical, &mut vars);
                }
            }
            Stmt::ExportAll {
                exported: Some(n), ..
            } => exported.push(n.clone()),
            Stmt::ExportNamed { specs, source } => {
                for s in specs {
                    exported.push(s.exported.clone());
                    if source.is_none() {
                        export_locals.push(s.local.clone());
                    }
                }
            }
            Stmt::ExportDecl(inner) => {
                let mut names = Vec::new();
                decl_bound_names(inner, &mut names);
                for n in &names {
                    exported.push(n.clone());
                }
                collect_top_decl(inner, &mut lexical, &mut vars);
            }
            other => collect_top_decl(other, &mut lexical, &mut vars),
        }
    }

    if let Some(dup) = first_duplicate(&exported) {
        return Err(ParseError {
            message: format!("duplicate export name '{dup}'"),
            line: 0,
        });
    }
    if let Some(dup) = first_duplicate(&lexical) {
        return Err(ParseError {
            message: format!("Identifier '{dup}' has already been declared"),
            line: 0,
        });
    }
    for v in &vars {
        if lexical.iter().any(|l| l == v) {
            return Err(ParseError {
                message: format!("Identifier '{v}' has already been declared"),
                line: 0,
            });
        }
    }
    for local in &export_locals {
        if !lexical.iter().any(|l| l == local) && !vars.iter().any(|v| v == local) {
            return Err(ParseError {
                message: format!("export '{local}' is not declared in the module"),
                line: 0,
            });
        }
    }
    Ok(())
}

/// A function body's top-level lexically-declared name may not also be a formal parameter.
fn params_body_lexical_clash(params: &[Param], body: &[Stmt]) -> Option<String> {
    let mut lexical = Vec::new();
    let mut vars = Vec::new();
    for stmt in body {
        collect_top_decl(stmt, &mut lexical, &mut vars);
    }
    if lexical.is_empty() {
        return None;
    }
    let pnames = param_names(params);
    lexical.into_iter().find(|l| pnames.iter().any(|p| p == l))
}

fn first_duplicate(names: &[String]) -> Option<String> {
    let mut seen = Vec::new();
    for n in names {
        if seen.contains(n) {
            return Some(n.clone());
        }
        seen.push(n.clone());
    }
    None
}

/// Names bound by a top-level declaration statement (for ExportedNames of `export <decl>`).
fn decl_bound_names(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::VarDecl { decls, .. } => {
            for (pat, _) in decls {
                pattern_names(pat, out);
            }
        }
        Stmt::FuncDecl(f) => out.extend(f.name.clone()),
        Stmt::ClassDecl(c) => out.extend(c.name.clone()),
        _ => {}
    }
}

/// Partition a top-level declaration's bound names into lexical (let/const/class/function) vs var.
fn collect_top_decl(stmt: &Stmt, lexical: &mut Vec<String>, vars: &mut Vec<String>) {
    match stmt {
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (pat, _) in decls {
                pattern_names(pat, vars);
            }
        }
        Stmt::VarDecl { decls, .. } => {
            for (pat, _) in decls {
                pattern_names(pat, lexical);
            }
        }
        Stmt::FuncDecl(f) => lexical.extend(f.name.clone()),
        Stmt::ClassDecl(c) => lexical.extend(c.name.clone()),
        _ => {}
    }
}

/// Recursion-depth ceiling for the parser. Beyond this we bail with a SyntaxError rather than
/// overflow the native stack on pathologically nested input (test262 has deeply-nested fixtures).
const MAX_PARSE_DEPTH: u32 = 1200;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    strict: bool,
    depth: u32,
    /// Whether the body currently being parsed is a generator / async function — controls whether
    /// `yield` / `await` are keywords here.
    in_generator: bool,
    in_async: bool,
    /// Set while parsing a formal-parameter list: an `await` (async) or `yield` (generator)
    /// expression is not allowed in a parameter default, so this poisons those expressions.
    in_params: bool,
    /// Suppress `in` as a binary operator (the `[NoIn]` grammar productions in a `for` head, before
    /// `in`/`of` is reached). Reset inside any bracketed/parenthesized sub-expression.
    no_in: bool,
    /// Whether the top-level goal is a Module (so `import`/`export` declarations are allowed).
    module: bool,
    /// Context depths for early-error checks: `return` requires a function, `continue` an iteration,
    /// `break` an iteration or switch.
    fn_depth: u32,
    iter_depth: u32,
    switch_depth: u32,
    /// Active labels in scope (reset at function boundaries).
    labels: Vec<String>,
    /// Per-scope declared names, for detecting lexical redeclaration.
    decl_scopes: Vec<DeclScope>,
    /// Set just before parsing a function body so the scope it pushes is marked a var boundary.
    next_scope_is_fn_boundary: bool,
    /// Seeds `new.target` validity at the top level of an `eval` whose caller is inside a function
    /// (direct eval in function code). `return`/`super()` stay gated separately, since they remain
    /// illegal in eval code even there.
    allow_new_target: bool,
    /// True while parsing a top-level `ModuleItem` / script statement — the only position where an
    /// `import`/`export` declaration is grammatically valid. Nested statement contexts clear it.
    top_level: bool,
    /// True where a `SuperProperty` (`super.x` / `super[e]`) is syntactically allowed: inside a
    /// method body, class field initializer, or class static block. An ordinary function clears it;
    /// an arrow inherits it. `super` outside any such context is a SyntaxError.
    super_prop_ok: bool,
    /// Deferred duplicate-`__proto__` errors from object literals. The Annex B early error does not
    /// apply when the literal turns out to be a destructuring assignment pattern, so `parse_assign`
    /// forgives entries recorded inside a reinterpreted pattern; leftovers surface at the end of the
    /// enclosing top-level statement.
    proto_dups: Vec<ParseError>,
    /// True when the expression just produced was a parenthesized primary (`(...)` with nothing
    /// consumed after it). Only used to let parens satisfy the `??` / `&&`,`||` no-mixing rule.
    last_paren: bool,
    /// Set for the immediately following `parse_stmt` when it parses a single-statement body
    /// (`if`/loop/`with`/label): there, `let` only commits to a declaration before `[`.
    single_stmt: bool,
}

#[derive(Default)]
struct DeclScope {
    lexical: Vec<String>,
    var: Vec<String>,
    /// A `var` hoists up to (and conflicts with lexicals only up to) the nearest such boundary — the
    /// program/function top. Block / `for` / `switch` scopes are not boundaries.
    fn_boundary: bool,
}

impl Parser {
    fn cur(&self) -> &Tok {
        &self.toks[self.pos].kind
    }
    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }
    fn nl_before(&self) -> bool {
        self.toks[self.pos].nl_before
    }
    /// Whether the current token contained a `\u` escape (so it can't be a contextual keyword).
    fn cur_escaped(&self) -> bool {
        self.toks[self.pos].escaped
    }
    fn at_eof(&self) -> bool {
        matches!(self.cur(), Tok::Eof)
    }
    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].kind.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: msg.into(),
            line: self.line(),
        })
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.cur(), Tok::Punct(x) if *x == p)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.cur(), Tok::Keyword(x) if *x == k)
    }
    /// True for a contextual keyword (`let`, `of`, `async`, ...) carried as an `Ident`.
    fn is_ident_word(&self, w: &str) -> bool {
        matches!(self.cur(), Tok::Ident(x) if x == w)
    }
    fn eat_punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_punct(&mut self, p: &str) -> Result<(), ParseError> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            self.err(format!("expected '{p}'"))
        }
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn eat_ident_word(&mut self, w: &str) -> bool {
        if self.is_ident_word(w) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Register a function declaration's name. In a *block*, a generator/async-function declaration is
    /// a lexical binding (so it conflicts on redeclaration); a plain function keeps Annex B var-like
    /// semantics. At a function/program top level every function declaration is var-like.
    fn declare_fn_decl(
        &mut self,
        name: &str,
        is_async: bool,
        is_generator: bool,
    ) -> Result<(), ParseError> {
        let in_block = !self.decl_scopes.last().unwrap().fn_boundary;
        if in_block && (self.strict || is_async || is_generator) {
            // Block-level functions are lexical; only sloppy plain functions get the Annex B
            // var-like treatment (which permits duplicates and shadowing).
            self.declare_lexical(name)
        } else {
            self.declare_var(name, false)
        }
    }

    /// A property shorthand (`{ x }` / `{ x = d }`) binds/references `x`, so the name must be a valid
    /// identifier — not a reserved word (even one spelled with a `\u` escape, which lexes as a keyword).
    fn check_shorthand_ident(&self, name: &str) -> Result<(), ParseError> {
        if KEYWORDS.contains(&name) {
            return self.err(format!(
                "'{name}' is a reserved word and cannot be a shorthand property"
            ));
        }
        if self.strict && is_strict_reserved_binding(name) {
            return self.err(format!(
                "'{name}' cannot be used as a shorthand property in strict mode"
            ));
        }
        if (self.in_async && name == "await") || (self.in_generator && name == "yield") {
            return self.err(format!(
                "'{name}' cannot be used as a shorthand property here"
            ));
        }
        Ok(())
    }

    fn push_decl_scope(&mut self) {
        let fn_boundary = std::mem::take(&mut self.next_scope_is_fn_boundary);
        self.decl_scopes.push(DeclScope {
            fn_boundary,
            ..Default::default()
        });
    }
    fn pop_decl_scope(&mut self) {
        self.decl_scopes.pop();
    }
    /// Record a lexical (`let`/`const`/`class`) binding; error on redeclaration in this scope.
    fn declare_lexical(&mut self, name: &str) -> Result<(), ParseError> {
        let conflict = {
            let s = self.decl_scopes.last().unwrap();
            s.lexical.iter().any(|n| n == name) || s.var.iter().any(|n| n == name)
        };
        if conflict {
            return self.err(format!("Identifier '{name}' has already been declared"));
        }
        self.decl_scopes
            .last_mut()
            .unwrap()
            .lexical
            .push(name.to_string());
        Ok(())
    }
    /// Record a `var`/function binding. A `var` (`hoist_through = true`) conflicts with a lexical of
    /// the same name in any block scope it hoists through — from here up to (and including) the
    /// nearest function/program boundary. A block-level *function* declaration (`hoist_through =
    /// false`) only conflicts in its own scope (Annex B.3.3 lets it shadow an enclosing lexical).
    fn declare_var(&mut self, name: &str, hoist_through: bool) -> Result<(), ParseError> {
        for scope in self.decl_scopes.iter().rev() {
            if scope.lexical.iter().any(|n| n == name) {
                return self.err(format!("Identifier '{name}' has already been declared"));
            }
            if scope.fn_boundary || !hoist_through {
                break;
            }
        }
        self.decl_scopes
            .last_mut()
            .unwrap()
            .var
            .push(name.to_string());
        Ok(())
    }

    fn has_use_strict_prologue(&self) -> bool {
        // Only a leading run of string-literal expression statements counts as the directive
        // prologue. The first one being exactly "use strict" enables strict mode.
        matches!(self.toks.first().map(|t| &t.kind), Some(Tok::Str(s)) if s == "use strict")
    }

    fn parse_stmts_until_eof(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut out = Vec::new();
        while !self.at_eof() {
            self.top_level = true;
            out.push(self.parse_stmt()?);
            // Any duplicate-__proto__ record not forgiven by a destructuring reinterpretation
            // inside this statement is a genuine object-literal early error.
            if let Some(e) = self.proto_dups.pop() {
                return Err(e);
            }
        }
        Ok(out)
    }

    // ----- statements -------------------------------------------------------------------------

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        // `import`/`export` declarations are valid only as top-level ModuleItems. Capture the flag
        // and clear it: any statement we recurse into (block, if/loop body, label, switch case) is a
        // nested context where an import/export declaration is a SyntaxError.
        let at_top = self.top_level;
        self.top_level = false;
        let single_stmt = std::mem::take(&mut self.single_stmt);
        match self.cur().clone() {
            Tok::Punct("{") => {
                self.advance();
                let body = self.parse_block_body()?;
                Ok(Stmt::Block(body))
            }
            Tok::Punct(";") => {
                self.advance();
                Ok(Stmt::Empty)
            }
            Tok::Keyword("var") => self.parse_var_decl(DeclKind::Var),
            Tok::Keyword("const") => self.parse_var_decl(DeclKind::Const),
            // In a single-statement (substatement) body only `let [` commits to a declaration —
            // ExpressionStatement's lookahead restriction is `let [` alone, so a bare `let` there
            // is an identifier reference (with ASI before a newline-separated binding name).
            Tok::Ident(w)
                if w == "let"
                    && self.starts_let_decl()
                    && (!single_stmt || matches!(self.peek_kind(1), Tok::Punct("["))) =>
            {
                self.parse_var_decl(DeclKind::Let)
            }
            Tok::Ident(w) if w == "using" && self.starts_using_decl() => {
                self.parse_var_decl(DeclKind::Using)
            }
            Tok::Ident(w) if w == "await" && self.in_async && self.starts_await_using() => {
                self.parse_var_decl(DeclKind::AwaitUsing)
            }
            Tok::Keyword("function") => {
                let f = self.parse_function(false, false)?;
                // An Annex B substatement-position function (`if (x) function f(){}`) binds in
                // its own implicit block: its name never conflicts with enclosing declarations.
                if !single_stmt {
                    if let Some(n) = &f.name {
                        self.declare_fn_decl(n, f.is_async, f.is_generator)?;
                    }
                }
                Ok(Stmt::FuncDecl(Rc::new(f)))
            }
            // `async function f(){}` declaration (async is a contextual keyword).
            Tok::Ident(w)
                if w == "async"
                    && !self.cur_escaped()
                    && matches!(self.peek_kind(1), Tok::Keyword("function")) =>
            {
                self.advance();
                let f = self.parse_function(true, false)?;
                if !single_stmt {
                    if let Some(n) = &f.name {
                        self.declare_fn_decl(n, f.is_async, f.is_generator)?;
                    }
                }
                Ok(Stmt::FuncDecl(Rc::new(f)))
            }
            Tok::Keyword("class") | Tok::Punct("@") => {
                let c = self.parse_class()?;
                if let Some(n) = &c.name {
                    self.declare_lexical(n)?;
                }
                Ok(Stmt::ClassDecl(Rc::new(c)))
            }
            Tok::Keyword("if") => self.parse_if(),
            Tok::Keyword("while") => self.parse_while(),
            Tok::Keyword("with") => self.parse_with(),
            Tok::Keyword("do") => self.parse_do_while(),
            Tok::Keyword("for") => self.parse_for(),
            Tok::Keyword("return") => {
                self.advance();
                if self.fn_depth == 0 {
                    return self.err("'return' outside of a function");
                }
                let arg = if self.can_end_stmt() {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.consume_semicolon()?;
                Ok(Stmt::Return(arg))
            }
            Tok::Keyword("break") => {
                self.advance();
                let label = self.parse_opt_label();
                match &label {
                    Some(l) if !self.labels.contains(l) => {
                        return self.err("undefined break label")
                    }
                    None if self.iter_depth == 0 && self.switch_depth == 0 => {
                        return self.err("illegal 'break' statement");
                    }
                    _ => {}
                }
                self.consume_semicolon()?;
                Ok(Stmt::Break(label))
            }
            Tok::Keyword("continue") => {
                self.advance();
                let label = self.parse_opt_label();
                match &label {
                    Some(l) if !self.labels.contains(l) => {
                        return self.err("undefined continue label")
                    }
                    None if self.iter_depth == 0 => {
                        return self.err("illegal 'continue' statement")
                    }
                    _ => {}
                }
                self.consume_semicolon()?;
                Ok(Stmt::Continue(label))
            }
            Tok::Keyword("throw") => {
                self.advance();
                if self.nl_before() {
                    return self.err("illegal newline after throw");
                }
                let arg = self.parse_expr()?;
                self.consume_semicolon()?;
                Ok(Stmt::Throw(arg))
            }
            Tok::Keyword("try") => self.parse_try(),
            // `import …` declaration (but `import(` / `import.meta` are expressions).
            Tok::Keyword("import")
                if at_top
                    && self.module
                    && !matches!(self.peek_kind(1), Tok::Punct("(") | Tok::Punct(".")) =>
            {
                self.parse_import()
            }
            Tok::Keyword("export") if at_top && self.module => self.parse_export(),
            // An `export`/`import` declaration nested in a statement position is a SyntaxError.
            Tok::Keyword("export")
                if !matches!(self.peek_kind(1), Tok::Punct("(") | Tok::Punct(".")) =>
            {
                self.err("'export' declaration may only appear at the top level of a module")
            }
            Tok::Keyword("import")
                if self.module
                    && !matches!(self.peek_kind(1), Tok::Punct("(") | Tok::Punct(".")) =>
            {
                self.err("'import' declaration may only appear at the top level of a module")
            }
            Tok::Keyword("switch") => self.parse_switch(),
            Tok::Keyword("debugger") => {
                self.advance();
                self.consume_semicolon()?;
                Ok(Stmt::Debugger)
            }
            // Labeled statement: `ident :` with the ident not being a known expression start that
            // would otherwise consume the colon.
            Tok::Ident(name) if matches!(self.peek_kind(1), Tok::Punct(":")) => {
                if (self.in_async && name == "await") || (self.in_generator && name == "yield") {
                    return self.err(format!("'{name}' cannot be used as a label here"));
                }
                self.advance();
                self.advance();
                if self.labels.contains(&name) {
                    return self.err(format!("label '{name}' has already been declared"));
                }
                self.labels.push(name.clone());
                let body = self.parse_substatement(true);
                self.labels.pop();
                Ok(Stmt::Labeled {
                    label: name,
                    body: Box::new(body?),
                })
            }
            _ => {
                let e = self.parse_expr()?;
                self.consume_semicolon()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn peek_kind(&self, ahead: usize) -> Tok {
        self.toks
            .get(self.pos + ahead)
            .map(|t| t.kind.clone())
            .unwrap_or(Tok::Eof)
    }

    /// After `let`, decide whether this is a `let` declaration (vs `let` used as an identifier).
    fn starts_let_decl(&self) -> bool {
        matches!(
            self.peek_kind(1),
            Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")
        )
    }
    /// Whether token `pos + ahead` is preceded by a line terminator.
    fn nl_at(&self, ahead: usize) -> bool {
        self.toks
            .get(self.pos + ahead)
            .map(|t| t.nl_before)
            .unwrap_or(false)
    }
    /// `using x` is a declaration when an identifier follows on the same line (no `[no LT]` break);
    /// otherwise `using` is just an identifier reference.
    fn starts_using_decl(&self) -> bool {
        matches!(self.peek_kind(1), Tok::Ident(_)) && !self.nl_at(1)
    }
    /// `await using x` (async context only): `using` then a binding identifier, all on one line.
    fn starts_await_using(&self) -> bool {
        matches!(self.peek_kind(1), Tok::Ident(w) if w == "using")
            && matches!(self.peek_kind(2), Tok::Ident(_))
            && !self.nl_at(1)
            && !self.nl_at(2)
    }

    fn parse_block_body(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.push_decl_scope();
        let mut out = Vec::new();
        let r = (|| {
            while !self.is_punct("}") && !self.at_eof() {
                out.push(self.parse_stmt()?);
            }
            self.expect_punct("}")
        })();
        self.pop_decl_scope();
        r?;
        Ok(out)
    }

    fn parse_var_decl(&mut self, kind: DeclKind) -> Result<Stmt, ParseError> {
        self.advance(); // var/let/const/using keyword (or `let`/`using`/`await` ident)
        if kind == DeclKind::AwaitUsing {
            self.advance(); // the `using` after `await`
        }
        let decls = self.parse_var_declarators()?;
        // A `const` declaration must have an initializer for each binding.
        if kind == DeclKind::Const && decls.iter().any(|(_, init)| init.is_none()) {
            return self.err("missing initializer in const declaration");
        }
        // `using`/`await using` bindings must be plain identifiers, each with an initializer.
        if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
            for (pat, init) in &decls {
                if !matches!(pat, Pattern::Ident(_)) {
                    return self.err("using declaration requires identifier bindings");
                }
                if init.is_none() {
                    return self.err("missing initializer in using declaration");
                }
            }
        }
        // Track declared names to catch lexical redeclaration.
        let mut names = Vec::new();
        for (pat, _) in &decls {
            pattern_names(pat, &mut names);
        }
        for n in &names {
            match kind {
                DeclKind::Var => self.declare_var(n, true)?,
                _ => self.declare_lexical(n)?,
            }
        }
        self.consume_semicolon()?;
        Ok(Stmt::VarDecl { kind, decls })
    }

    fn parse_var_declarators(&mut self) -> Result<Vec<(Pattern, Option<Expr>)>, ParseError> {
        let mut decls = Vec::new();
        loop {
            let pat = self.parse_binding_pattern()?;
            let init = if self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            decls.push((pat, init));
            if !self.eat_punct(",") {
                break;
            }
        }
        Ok(decls)
    }

    fn parse_binding_ident(&mut self) -> Result<Pattern, ParseError> {
        Ok(Pattern::Ident(self.parse_binding_ident_name()?))
    }

    fn parse_binding_ident_name(&mut self) -> Result<String, ParseError> {
        match self.cur().clone() {
            Tok::Ident(name) => {
                // In strict mode `eval`/`arguments` and the strict-reserved words can't be bound.
                if self.strict && is_strict_reserved_binding(&name) {
                    return self.err(format!(
                        "'{name}' cannot be used as a binding in strict mode"
                    ));
                }
                // `await`/`yield` are reserved as bindings inside async/generator bodies.
                if (self.in_async && name == "await") || (self.in_generator && name == "yield") {
                    return self.err(format!("'{name}' cannot be used as a binding here"));
                }
                self.advance();
                Ok(name)
            }
            _ => self.err("expected binding identifier"),
        }
    }

    /// A binding target: a plain identifier, or an array/object destructuring pattern.
    fn parse_binding_pattern(&mut self) -> Result<Pattern, ParseError> {
        match self.cur() {
            Tok::Punct("[") => self.parse_array_pattern(),
            Tok::Punct("{") => self.parse_object_pattern(),
            _ => self.parse_binding_ident(),
        }
    }

    fn parse_array_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.expect_punct("[")?;
        let mut elems = Vec::new();
        while !self.is_punct("]") {
            if self.is_punct(",") {
                self.advance();
                elems.push(ArrayPatElem::Hole);
                continue;
            }
            if self.eat_punct("...") {
                let pat = self.parse_binding_pattern()?;
                elems.push(ArrayPatElem::Rest(pat));
                break;
            }
            let pattern = self.parse_binding_pattern()?;
            let default = if self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            elems.push(ArrayPatElem::Elem { pattern, default });
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("]")?;
        Ok(Pattern::Array(elems))
    }

    fn parse_object_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.expect_punct("{")?;
        let mut props = Vec::new();
        let mut rest = None;
        while !self.is_punct("}") {
            if self.eat_punct("...") {
                rest = Some(self.parse_binding_ident_name()?);
                break;
            }
            let key = self.parse_prop_key()?;
            let (value, default) = if self.eat_punct(":") {
                let v = self.parse_binding_pattern()?;
                let d = if self.eat_punct("=") {
                    Some(self.parse_assign()?)
                } else {
                    None
                };
                (v, d)
            } else {
                // Shorthand `{ a }` or `{ a = default }` — key must be a plain identifier.
                let name = match &key {
                    PropKey::Ident(n) => n.clone(),
                    _ => return self.err("invalid shorthand destructuring target"),
                };
                self.check_shorthand_ident(&name)?;
                let d = if self.eat_punct("=") {
                    Some(self.parse_assign()?)
                } else {
                    None
                };
                (Pattern::Ident(name), d)
            };
            props.push(ObjPatProp {
                key,
                value,
                default,
            });
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("}")?;
        Ok(Pattern::Object(ObjectPat { props, rest }))
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        self.expect_punct("(")?;
        let test = self.parse_expr()?;
        self.expect_punct(")")?;
        let cons = Box::new(self.parse_substatement(true)?);
        let alt = if self.eat_kw("else") {
            Some(Box::new(self.parse_substatement(true)?))
        } else {
            None
        };
        Ok(Stmt::If { test, cons, alt })
    }

    /// Parse a statement that is the body of `if`/loop/`with`/label — a lexical (`let`/`const`) or
    /// `class` declaration is not allowed there. A plain `FunctionDeclaration` is allowed only in the
    /// `if`/`else`/label positions in sloppy mode (Annex B); async/generator functions never are.
    fn parse_substatement(&mut self, annexb_fn: bool) -> Result<Stmt, ParseError> {
        self.single_stmt = true;
        let s = self.parse_stmt()?;
        // A label chain is transparent for these checks, and the Annex B allowance only covers a
        // *direct* FunctionDeclaration: `if (0) l: function f(){}` is illegal even in sloppy mode.
        let mut labelled = false;
        let mut inner = &s;
        while let Stmt::Labeled { body, .. } = inner {
            labelled = true;
            inner = body;
        }
        match inner {
            Stmt::VarDecl {
                kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                ..
            }
            | Stmt::ClassDecl(_) => {
                return self.err("lexical declaration cannot appear in a single-statement context");
            }
            Stmt::FuncDecl(f)
                if labelled || f.is_async || f.is_generator || !annexb_fn || self.strict =>
            {
                return self
                    .err("function declaration cannot appear in a single-statement context");
            }
            _ => {}
        }
        Ok(s)
    }

    /// Parse a loop body inside an iteration context (so `break`/`continue` are legal).
    fn parse_loop_body(&mut self) -> Result<Stmt, ParseError> {
        self.iter_depth += 1;
        let r = self.parse_substatement(false);
        self.iter_depth -= 1;
        r
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        self.expect_punct("(")?;
        let test = self.parse_expr()?;
        self.expect_punct(")")?;
        let body = Box::new(self.parse_loop_body()?);
        Ok(Stmt::While { test, body })
    }

    fn parse_with(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        if self.strict {
            return self.err("'with' statements are not allowed in strict mode");
        }
        self.expect_punct("(")?;
        let obj = self.parse_expr()?;
        self.expect_punct(")")?;
        let body = Box::new(self.parse_substatement(false)?);
        Ok(Stmt::With { obj, body })
    }

    fn parse_do_while(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let body = Box::new(self.parse_loop_body()?);
        if !self.eat_kw("while") {
            return self.err("expected 'while' after do-body");
        }
        self.expect_punct("(")?;
        let test = self.parse_expr()?;
        self.expect_punct(")")?;
        self.eat_punct(";");
        Ok(Stmt::DoWhile { body, test })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        // A `for (let … )` head shares one lexical scope with the body.
        self.push_decl_scope();
        let r = self.parse_for_inner();
        self.pop_decl_scope();
        r
    }

    fn parse_for_inner(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let is_await = self.eat_ident_word("await");
        if is_await && !self.in_async {
            return self.err("'for await' is only valid in an async context");
        }
        self.expect_punct("(")?;

        // Determine the head form. Parse an optional declaration kind or init expression, then look
        // for `in`/`of`.
        let decl_kind = if self.is_kw("var") {
            self.advance();
            Some(DeclKind::Var)
        } else if self.is_kw("const") {
            self.advance();
            Some(DeclKind::Const)
        } else if self.is_ident_word("let") && self.starts_let_decl() {
            self.advance();
            Some(DeclKind::Let)
        } else if self.is_ident_word("using") && self.starts_using_decl() {
            self.advance();
            Some(DeclKind::Using)
        } else if self.is_ident_word("await") && self.in_async && self.starts_await_using() {
            self.advance(); // await
            self.advance(); // using
            Some(DeclKind::AwaitUsing)
        } else {
            None
        };

        if let Some(kind) = decl_kind {
            let is_using = matches!(kind, DeclKind::Using | DeclKind::AwaitUsing);
            let first = self.parse_binding_pattern()?;
            if self.is_kw("in") || (self.is_ident_word("of") && !self.cur_escaped()) {
                let of = self.is_ident_word("of") && !self.cur_escaped();
                // `using`/`await using` are only valid in a for-of head, never for-in.
                if is_using && !of {
                    return self.err("'using' declaration is not allowed in a for-in head");
                }
                self.advance();
                // A let/const/using for-in/of head binds into the shared loop scope: its bound names
                // must be unique and must not clash with a `var` in the body.
                if matches!(
                    kind,
                    DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing
                ) {
                    let mut names = Vec::new();
                    pattern_names(&first, &mut names);
                    for n in &names {
                        self.declare_lexical(n)?;
                    }
                }
                let right = self.parse_assign()?;
                self.expect_punct(")")?;
                let body = Box::new(self.parse_loop_body()?);
                return Ok(Stmt::ForInOf {
                    decl: Some(kind),
                    left: first,
                    right,
                    of,
                    is_await,
                    body,
                });
            }
            // Plain C-style for with a declaration init (possibly multiple declarators).
            let init_expr = if self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            let mut decls = vec![(first, init_expr)];
            while self.eat_punct(",") {
                let pat = self.parse_binding_pattern()?;
                let init = if self.eat_punct("=") {
                    Some(self.parse_assign()?)
                } else {
                    None
                };
                decls.push((pat, init));
            }
            self.expect_punct(";")?;
            return self.finish_c_for(Some(Box::new(ForInit::VarDecl { kind, decls })));
        }

        // No declaration: either empty init or an expression init.
        if self.eat_punct(";") {
            return self.finish_c_for(None);
        }
        let init_expr = self.parse_expr_no_in()?;
        if self.is_kw("in") || (self.is_ident_word("of") && !self.cur_escaped()) {
            let of = self.is_ident_word("of") && !self.cur_escaped();
            self.advance();
            let right = self.parse_assign()?;
            self.expect_punct(")")?;
            let body = Box::new(self.parse_loop_body()?);
            let left = expr_to_pattern(&init_expr).ok_or_else(|| ParseError {
                message: "invalid for-in/of target".into(),
                line: self.line(),
            })?;
            return Ok(Stmt::ForInOf {
                decl: None,
                left,
                right,
                of,
                is_await,
                body,
            });
        }
        self.expect_punct(";")?;
        self.finish_c_for(Some(Box::new(ForInit::Expr(init_expr))))
    }

    fn finish_c_for(&mut self, init: Option<Box<ForInit>>) -> Result<Stmt, ParseError> {
        let test = if self.is_punct(";") {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_punct(";")?;
        let update = if self.is_punct(")") {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_punct(")")?;
        let body = Box::new(self.parse_loop_body()?);
        Ok(Stmt::For {
            init,
            test,
            update,
            body,
        })
    }

    /// A ModuleExportName: an identifier/keyword, or a string literal (`export { x as "y" }`).
    fn parse_module_export_name(&mut self) -> Result<String, ParseError> {
        match self.cur().clone() {
            Tok::Str(s) => {
                // A StringLiteral ModuleExportName must be well-formed Unicode (no lone surrogates).
                if self.toks[self.pos].lone_surrogate {
                    return self.err("module export name must be well-formed Unicode");
                }
                self.advance();
                Ok(s)
            }
            _ => self.parse_property_name_ident(),
        }
    }
    fn expect_keyword_word(&mut self, w: &str) -> Result<(), ParseError> {
        if self.is_ident_word(w) {
            self.advance();
            Ok(())
        } else {
            self.err(format!("expected '{w}'"))
        }
    }
    fn parse_module_specifier(&mut self) -> Result<Rc<str>, ParseError> {
        let spec = match self.cur().clone() {
            Tok::Str(s) => {
                self.advance();
                Rc::from(s.as_str())
            }
            _ => return self.err("expected a module specifier string"),
        };
        self.parse_import_attributes()?;
        Ok(spec)
    }
    /// Parse (and validate) an optional import-attributes clause: `with { key: "value", … }` (or the
    /// legacy `assert { … }`). The attribute values must be string literals and the keys must be
    /// unique — a duplicate key is an early SyntaxError.
    fn parse_import_attributes(&mut self) -> Result<(), ParseError> {
        if !self.is_kw("with") && !self.is_ident_word("assert") {
            return Ok(());
        }
        self.advance(); // 'with' / 'assert'
        self.expect_punct("{")?;
        let mut keys: Vec<String> = Vec::new();
        while !self.is_punct("}") {
            let key = match self.cur().clone() {
                Tok::Str(s) => {
                    self.advance();
                    s
                }
                _ => self.parse_property_name_ident()?,
            };
            if keys.contains(&key) {
                return self.err(format!("duplicate import attribute key '{key}'"));
            }
            keys.push(key);
            self.expect_punct(":")?;
            if !matches!(self.cur(), Tok::Str(_)) {
                return self.err("import attribute value must be a string literal");
            }
            self.advance();
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("}")?;
        Ok(())
    }

    /// Parse the `( specifier [, options] )` of a dynamic-import call (any phase). The optional
    /// second options argument is accepted and ignored.
    fn parse_import_call_args(&mut self) -> Result<Expr, ParseError> {
        self.expect_punct("(")?;
        let spec = self.parse_assign_allow_in()?;
        if self.eat_punct(",") && !self.is_punct(")") {
            let _ = self.parse_assign_allow_in()?;
            self.eat_punct(",");
        }
        self.expect_punct(")")?;
        Ok(spec)
    }

    fn parse_import(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // 'import'
                        // Bare import: `import "spec";`
        if let Tok::Str(s) = self.cur().clone() {
            self.advance();
            let source = Rc::from(s.as_str());
            self.parse_import_attributes()?;
            self.consume_semicolon()?;
            return Ok(Stmt::Import(ImportDecl {
                source,
                specs: Vec::new(),
            }));
        }
        let mut specs = Vec::new();
        let mut need_from = true;
        // Default binding.
        if matches!(self.cur(), Tok::Ident(_)) {
            specs.push(ImportSpec::Default(self.parse_binding_ident_name()?));
            if !self.eat_punct(",") {
                need_from = true;
            }
        }
        if self.eat_punct("*") {
            self.expect_keyword_word("as")?;
            specs.push(ImportSpec::Namespace(self.parse_binding_ident_name()?));
        } else if self.is_punct("{") {
            self.advance();
            while !self.is_punct("}") {
                let imported_is_string = matches!(self.cur(), Tok::Str(_));
                let imported = self.parse_module_export_name()?;
                let local = if self.is_ident_word("as") {
                    self.advance();
                    self.parse_binding_ident_name()?
                } else {
                    // Without `as`, the imported name is also the local BindingIdentifier: it must be
                    // a plain identifier (not a string module export name) and a legal strict binding.
                    if imported_is_string {
                        return self.err("imported binding name must be an identifier");
                    }
                    if is_strict_reserved_binding(&imported) {
                        return self.err(format!(
                            "'{imported}' cannot be used as a binding in strict mode"
                        ));
                    }
                    imported.clone()
                };
                specs.push(ImportSpec::Named { imported, local });
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}")?;
        }
        let _ = need_from;
        self.expect_keyword_word("from")?;
        let source = self.parse_module_specifier()?;
        self.consume_semicolon()?;
        Ok(Stmt::Import(ImportDecl { source, specs }))
    }

    fn parse_export(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // 'export'
                        // export default …
        if self.eat_kw("default") {
            let stmt = if self.is_kw("function")
                || (self.is_ident_word("async")
                    && matches!(self.peek_kind(1), Tok::Keyword("function")))
            {
                let is_async = self.eat_ident_word("async");
                Stmt::FuncDecl(Rc::new(self.parse_function(is_async, false)?))
            } else if self.is_kw("class") {
                Stmt::ClassDecl(Rc::new(self.parse_class()?))
            } else {
                let e = self.parse_assign()?;
                self.consume_semicolon()?;
                Stmt::Expr(e)
            };
            return Ok(Stmt::ExportDefault(Box::new(stmt)));
        }
        // export * [as ns] from "spec"
        if self.eat_punct("*") {
            let exported = if self.is_ident_word("as") {
                self.advance();
                Some(self.parse_module_export_name()?)
            } else {
                None
            };
            self.expect_keyword_word("from")?;
            let source = self.parse_module_specifier()?;
            self.consume_semicolon()?;
            return Ok(Stmt::ExportAll { source, exported });
        }
        // export { a, b as c } [from "spec"]
        if self.is_punct("{") {
            self.advance();
            let mut specs = Vec::new();
            // Track which `local` names were string literals: without a `from` clause, a local name
            // must be a plain IdentifierReference (a string module export name is only valid as the
            // re-exported *source* name of an `export … from`).
            let mut string_locals: Vec<String> = Vec::new();
            while !self.is_punct("}") {
                let local_is_string = matches!(self.cur(), Tok::Str(_));
                let local = self.parse_module_export_name()?;
                if local_is_string {
                    string_locals.push(local.clone());
                }
                let exported = if self.is_ident_word("as") {
                    self.advance();
                    self.parse_module_export_name()?
                } else {
                    local.clone()
                };
                specs.push(ExportSpec { local, exported });
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}")?;
            let source = if self.is_ident_word("from") {
                self.advance();
                Some(self.parse_module_specifier()?)
            } else {
                None
            };
            if source.is_none() {
                if let Some(name) = string_locals.first() {
                    return self.err(format!(
                        "'{name}' is a string literal and cannot be an exported local binding"
                    ));
                }
            }
            self.consume_semicolon()?;
            return Ok(Stmt::ExportNamed { specs, source });
        }
        // export const/let/var/function/class …
        let decl = self.parse_stmt()?;
        Ok(Stmt::ExportDecl(Box::new(decl)))
    }

    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        self.expect_punct("{")?;
        let block = self.parse_block_body()?;
        let handler = if self.eat_kw("catch") {
            let param = if self.eat_punct("(") {
                let p = self.parse_binding_pattern()?;
                self.expect_punct(")")?;
                // A destructuring catch parameter must bind each name only once.
                let mut names = Vec::new();
                pattern_names(&p, &mut names);
                if let Some(dup) = duplicate_name(&names) {
                    return self.err(format!("duplicate binding '{dup}' in catch parameter"));
                }
                Some(p)
            } else {
                None
            };
            self.expect_punct("{")?;
            let body = self.parse_block_body()?;
            // A catch parameter name can't be re-declared by a lexical (`let`/`const`/`class`) in the
            // catch block (a `var` of the same name is allowed — Annex B.3.4).
            if let Some(p) = &param {
                let mut params = Vec::new();
                pattern_names(p, &mut params);
                let (mut lexical, mut vars) = (Vec::new(), Vec::new());
                for s in &body {
                    collect_top_decl(s, &mut lexical, &mut vars);
                }
                if let Some(name) = lexical.iter().find(|n| params.contains(n)) {
                    return self.err(format!("Identifier '{name}' has already been declared"));
                }
            }
            Some((param, body))
        } else {
            None
        };
        let finalizer = if self.eat_kw("finally") {
            self.expect_punct("{")?;
            Some(self.parse_block_body()?)
        } else {
            None
        };
        if handler.is_none() && finalizer.is_none() {
            return self.err("missing catch or finally after try");
        }
        Ok(Stmt::Try {
            block,
            handler,
            finalizer,
        })
    }

    fn parse_switch(&mut self) -> Result<Stmt, ParseError> {
        self.push_decl_scope(); // a switch body is one lexical scope shared by all cases
        let r = self.parse_switch_inner();
        self.pop_decl_scope();
        r
    }

    fn parse_switch_inner(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        self.expect_punct("(")?;
        let disc = self.parse_expr()?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        self.switch_depth += 1; // `break` is legal directly inside a switch
        let mut cases = Vec::new();
        while !self.is_punct("}") && !self.at_eof() {
            let test = if self.eat_kw("case") {
                let e = self.parse_expr()?;
                Some(e)
            } else if self.eat_kw("default") {
                None
            } else {
                self.switch_depth -= 1;
                return self.err("expected 'case' or 'default'");
            };
            self.expect_punct(":")?;
            let mut body = Vec::new();
            while !self.is_punct("}")
                && !self.is_kw("case")
                && !self.is_kw("default")
                && !self.at_eof()
            {
                body.push(self.parse_stmt()?);
            }
            cases.push(SwitchCase { test, body });
        }
        self.switch_depth -= 1;
        self.expect_punct("}")?;
        Ok(Stmt::Switch { disc, cases })
    }

    fn parse_opt_label(&mut self) -> Option<String> {
        if self.nl_before() {
            return None;
        }
        if let Tok::Ident(name) = self.cur().clone() {
            self.advance();
            Some(name)
        } else {
            None
        }
    }

    // ----- ASI -------------------------------------------------------------------------------

    fn can_end_stmt(&self) -> bool {
        self.is_punct(";") || self.is_punct("}") || self.at_eof() || self.nl_before()
    }

    fn consume_semicolon(&mut self) -> Result<(), ParseError> {
        if self.eat_punct(";") {
            return Ok(());
        }
        if self.is_punct("}") || self.at_eof() || self.nl_before() {
            return Ok(()); // automatic semicolon insertion
        }
        self.err("expected ';'")
    }

    // ----- expressions -----------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let first = self.parse_assign()?;
        if self.is_punct(",") {
            let mut seq = vec![first];
            while self.eat_punct(",") {
                seq.push(self.parse_assign()?);
            }
            Ok(Expr::Seq(seq))
        } else {
            Ok(first)
        }
    }

    /// Parse a sub-expression with `in` re-enabled (inside brackets/parens/args, where the for-head
    /// `[NoIn]` restriction no longer applies).
    fn parse_expr_allow_in(&mut self) -> Result<Expr, ParseError> {
        let saved = self.no_in;
        self.no_in = false;
        let e = self.parse_expr();
        self.no_in = saved;
        e
    }

    fn parse_assign_allow_in(&mut self) -> Result<Expr, ParseError> {
        let saved = self.no_in;
        self.no_in = false;
        let e = self.parse_assign();
        self.no_in = saved;
        e
    }

    fn parse_expr_no_in(&mut self) -> Result<Expr, ParseError> {
        // Parse the for-head initializer with `in` suppressed at the top level, so a bare
        // `for (x in obj)` detects the `in` keyword instead of consuming it as an operator.
        let saved = self.no_in;
        self.no_in = true;
        let e = self.parse_expr();
        self.no_in = saved;
        e
    }

    fn parse_assign(&mut self) -> Result<Expr, ParseError> {
        // `yield` / `yield*` (only a keyword inside a generator body).
        if self.in_generator && self.is_ident_word("yield") {
            return self.parse_yield();
        }
        // Arrow functions: `ident =>` or `( ... ) =>`.
        if let Some(arrow) = self.try_parse_arrow()? {
            return Ok(arrow);
        }
        let proto_mark = self.proto_dups.len();
        let left = self.parse_cond()?;
        if let Tok::Punct(op) = self.cur() {
            let op = *op;
            if is_assign_op(op) {
                self.advance();
                let value = self.parse_assign()?;
                // Plain `=` also accepts an array/object literal reinterpreted as a destructuring
                // assignment target — but only if it is a *valid* destructuring pattern.
                let destructuring = op == "=" && matches!(left, Expr::Array(_) | Expr::Object(_));
                if destructuring {
                    if !is_valid_assign_pattern(&left) {
                        return self.err("invalid destructuring assignment target");
                    }
                    // A pattern may repeat `__proto__:` — forgive dups recorded inside it.
                    self.proto_dups.truncate(proto_mark);
                } else if !is_valid_assign_target(&left) {
                    return self.err("invalid assignment target");
                }
                // Assigning to `eval`/`arguments` is a SyntaxError in strict mode.
                if self.strict {
                    if let Expr::Ident(n) = &left {
                        if n == "eval" || n == "arguments" {
                            return self
                                .err("cannot assign to 'eval' or 'arguments' in strict mode");
                        }
                    }
                }
                return Ok(Expr::Assign {
                    op,
                    target: Box::new(left),
                    value: Box::new(value),
                });
            }
        }
        Ok(left)
    }

    fn parse_yield(&mut self) -> Result<Expr, ParseError> {
        if self.in_params {
            return self.err("yield expression is not allowed in formal parameters");
        }
        self.advance(); // yield
                        // No LineTerminator is allowed between `yield` and `*` (ASI would split them).
        let delegate = !self.nl_before() && self.eat_punct("*");
        // A bare `yield` has no argument (before a line terminator or a token that can't start one).
        let no_arg = (!delegate && self.nl_before())
            || matches!(
                self.cur(),
                Tok::Punct(";" | ")" | "]" | "}" | "," | ":") | Tok::Eof
            );
        let arg = if no_arg {
            None
        } else {
            Some(Box::new(self.parse_assign()?))
        };
        Ok(Expr::Yield { delegate, arg })
    }

    fn parse_cond(&mut self) -> Result<Expr, ParseError> {
        let test = self.parse_binary(0)?;
        if self.eat_punct("?") {
            // The consequent is always `AssignmentExpression[+In]`; the alternative keeps the outer
            // `[?In]`, so a `for (a ? b in c : d ;;)` head allows `in` in the consequent branch.
            let cons = self.parse_assign_allow_in()?;
            self.expect_punct(":")?;
            let alt = self.parse_assign()?;
            Ok(Expr::Cond {
                test: Box::new(test),
                cons: Box::new(cons),
                alt: Box::new(alt),
            })
        } else {
            Ok(test)
        }
    }

    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        let mut left_paren = self.last_paren;
        while let Some((op, prec, right_assoc, logical)) = self.binary_op() {
            if prec < min_prec {
                break;
            }
            // ShortCircuitExpression: `??` cannot mix with `&&`/`||` without parentheses.
            if op == "??"
                && !left_paren
                && matches!(
                    &left,
                    Expr::Logical {
                        op: "&&" | "||",
                        ..
                    }
                )
            {
                return self.err("cannot mix '??' with '&&' or '||' without parentheses");
            }
            self.advance();
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_binary(next_min)?;
            if op == "??"
                && !self.last_paren
                && matches!(
                    &right,
                    Expr::Logical {
                        op: "&&" | "||",
                        ..
                    }
                )
            {
                return self.err("cannot mix '??' with '&&' or '||' without parentheses");
            }
            left_paren = false;
            self.last_paren = false;
            left = if logical {
                Expr::Logical {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            } else if op == "in" {
                // `#field in obj` is the ergonomic brand check, not a normal `in`.
                if let Expr::Ident(n) = &left {
                    if n.starts_with('#') {
                        left = Expr::PrivateIn {
                            name: n.clone(),
                            obj: Box::new(right),
                        };
                        continue;
                    }
                }
                Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            } else {
                Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            };
        }
        Ok(left)
    }

    /// Returns (operator, precedence, right-associative, is-logical) for the current token.
    fn binary_op(&self) -> Option<(&'static str, u8, bool, bool)> {
        let op = match self.cur() {
            Tok::Punct(p) => *p,
            Tok::Keyword("instanceof") => "instanceof",
            // `in` is not an operator in a `[NoIn]` context (the head of a `for` statement).
            Tok::Keyword("in") if self.no_in => return None,
            Tok::Keyword("in") => "in",
            _ => return None,
        };
        let (prec, right, logical) = match op {
            "??" => (1, false, true),
            "||" => (2, false, true),
            "&&" => (3, false, true),
            "|" => (4, false, false),
            "^" => (5, false, false),
            "&" => (6, false, false),
            "==" | "!=" | "===" | "!==" => (7, false, false),
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => (8, false, false),
            "<<" | ">>" | ">>>" => (9, false, false),
            "+" | "-" => (10, false, false),
            "*" | "/" | "%" => (11, false, false),
            "**" => (12, true, false),
            _ => return None,
        };
        Some((op, prec, right, logical))
    }

    /// Depth-guarded entry point. Every expression flows through `parse_unary` (each operand of a
    /// binary op, each parenthesised/array/object nesting level), so bracketing it here bounds all
    /// expression recursion with a single choke point.
    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return self.err("expression nesting too deep");
        }
        let r = self.parse_unary_inner();
        self.depth -= 1;
        r
    }

    fn parse_unary_inner(&mut self) -> Result<Expr, ParseError> {
        // `await expr` (only a keyword inside an async function body / module top level). A `\u`-
        // escaped spelling is never the `await` keyword, so it can't form an await expression.
        if self.in_async && self.is_ident_word("await") {
            if self.cur_escaped() {
                return self.err("'await' keyword must not contain escape sequences");
            }
            if self.in_params {
                return self.err("await expression is not allowed in formal parameters");
            }
            self.advance();
            let arg = self.parse_unary()?;
            // An await expression is a UnaryExpression: it cannot be an exponentiation base.
            if self.is_punct("**") {
                return self
                    .err("an await expression cannot be the base of '**' (use parentheses)");
            }
            return Ok(Expr::Await(Box::new(arg)));
        }
        let op = match self.cur() {
            Tok::Punct(p @ ("+" | "-" | "!" | "~")) => Some(*p),
            Tok::Keyword(k @ ("typeof" | "void" | "delete")) => Some(*k),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let arg = self.parse_unary()?;
            // Deleting a bare variable reference is a SyntaxError in strict mode.
            if op == "delete" && self.strict && matches!(arg, Expr::Ident(_)) {
                return self.err("delete of an unqualified identifier in strict mode");
            }
            // Deleting a private member (`delete obj.#x`) is always a SyntaxError.
            if op == "delete" && deletes_private_member(&arg) {
                return self.err("private fields cannot be deleted");
            }
            // A UnaryExpression cannot be an exponentiation base: `-x ** 2` is a SyntaxError.
            if self.is_punct("**") {
                return self.err("a unary expression cannot be the base of '**' (use parentheses)");
            }
            return Ok(Expr::Unary {
                op,
                arg: Box::new(arg),
            });
        }
        // Prefix ++/--
        if self.is_punct("++") || self.is_punct("--") {
            let op = if self.is_punct("++") { "++" } else { "--" };
            self.advance();
            let arg = self.parse_unary()?;
            self.check_strict_update_target(&arg)?;
            return Ok(Expr::Update {
                op,
                prefix: true,
                arg: Box::new(arg),
            });
        }
        self.parse_postfix()
    }

    fn check_strict_update_target(&self, arg: &Expr) -> Result<(), ParseError> {
        // The operand of `++`/`--` must be a simple assignment target (Identifier or member access).
        match arg {
            Expr::Ident(n) => {
                if self.strict && (n == "eval" || n == "arguments") {
                    return self
                        .err("cannot increment/decrement 'eval' or 'arguments' in strict mode");
                }
            }
            Expr::Member { .. } | Expr::Index { .. } => {}
            _ => return self.err("invalid increment/decrement operand"),
        }
        Ok(())
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let expr = self.parse_lhs()?;
        if !self.nl_before() && (self.is_punct("++") || self.is_punct("--")) {
            let op = if self.is_punct("++") { "++" } else { "--" };
            self.advance();
            self.check_strict_update_target(&expr)?;
            return Ok(Expr::Update {
                op,
                prefix: false,
                arg: Box::new(expr),
            });
        }
        Ok(expr)
    }

    fn parse_lhs(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_member_expr()?;
        let mut had_optional = false;
        loop {
            if self.is_punct("(") {
                let args = self.parse_args()?;
                expr = Expr::Call {
                    callee: Box::new(expr),
                    args,
                    optional: false,
                };
            } else if self.eat_punct(".") {
                let name = self.parse_property_name_ident()?;
                expr = Expr::Member {
                    obj: Box::new(expr),
                    prop: name,
                    optional: false,
                };
            } else if self.eat_punct("[") {
                let index = self.parse_expr_allow_in()?;
                self.expect_punct("]")?;
                expr = Expr::Index {
                    obj: Box::new(expr),
                    index: Box::new(index),
                    optional: false,
                };
            } else if let Tok::Template(parts) = self.cur().clone() {
                if had_optional {
                    return self.err("tagged template cannot appear in an optional chain");
                }
                // A template immediately after an expression is a tagged template.
                self.advance();
                expr = self.build_tagged_template(expr, parts)?;
            } else if self.eat_punct("?.") {
                had_optional = true;
                if self.is_punct("(") {
                    let args = self.parse_args()?;
                    expr = Expr::Call {
                        callee: Box::new(expr),
                        args,
                        optional: true,
                    };
                } else if self.eat_punct("[") {
                    let index = self.parse_expr_allow_in()?;
                    self.expect_punct("]")?;
                    expr = Expr::Index {
                        obj: Box::new(expr),
                        index: Box::new(index),
                        optional: true,
                    };
                } else {
                    let name = self.parse_property_name_ident()?;
                    expr = Expr::Member {
                        obj: Box::new(expr),
                        prop: name,
                        optional: true,
                    };
                }
            } else {
                break;
            }
        }
        // Wrap a chain that used `?.` so the whole thing short-circuits to undefined on a nullish link.
        if had_optional {
            expr = Expr::OptionalChain(Box::new(expr));
        }
        Ok(expr)
    }

    /// MemberExpression without trailing calls — handles `new` and `.`/`[]` member tails.
    fn parse_member_expr(&mut self) -> Result<Expr, ParseError> {
        let mut base = if self.is_kw("new") {
            self.advance();
            if self.eat_punct(".") {
                // `new.target` — only valid inside a function. (lumen models its value as undefined.)
                if !self.is_ident_word("target") {
                    return self.err("expected 'target' after 'new.'");
                }
                self.advance();
                if self.fn_depth == 0 && !self.allow_new_target {
                    return self.err("new.target is only valid inside a function");
                }
                Expr::NewTarget
            } else {
                // `new await …` is a SyntaxError where `await` is the keyword: an await expression is
                // a UnaryExpression, not the MemberExpression that `new` requires.
                if self.in_async && self.is_ident_word("await") && !self.cur_escaped() {
                    return self.err("'await' expression cannot be the operand of 'new'");
                }
                let callee = self.parse_member_expr()?;
                // `new import(...)` is a SyntaxError — an ImportCall (`import()`) is a CallExpression,
                // so it can't be the MemberExpression base of `new` (even under property access).
                if callee_has_import_call(&callee) {
                    return self.err("'import(...)' cannot be used with 'new'");
                }
                let args = if self.is_punct("(") {
                    self.parse_args()?
                } else {
                    Vec::new()
                };
                Expr::New {
                    callee: Box::new(callee),
                    args,
                }
            }
        } else {
            self.parse_primary()?
        };
        loop {
            if self.eat_punct(".") {
                let name = self.parse_property_name_ident()?;
                base = Expr::Member {
                    obj: Box::new(base),
                    prop: name,
                    optional: false,
                };
            } else if self.eat_punct("[") {
                let index = self.parse_expr_allow_in()?;
                self.expect_punct("]")?;
                base = Expr::Index {
                    obj: Box::new(base),
                    index: Box::new(index),
                    optional: false,
                };
            } else {
                break;
            }
        }
        Ok(base)
    }

    fn parse_args(&mut self) -> Result<Vec<ArrayElem>, ParseError> {
        self.expect_punct("(")?;
        let saved = self.no_in;
        self.no_in = false; // arguments are a fresh expression context
        let mut args = Vec::new();
        while !self.is_punct(")") {
            if self.eat_punct("...") {
                args.push(ArrayElem::Spread(self.parse_assign()?));
            } else {
                args.push(ArrayElem::Item(self.parse_assign()?));
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_punct(")")?;
        Ok(args)
    }

    /// A property name after `.`: any identifier or keyword is allowed (`x.if` is legal).
    fn parse_property_name_ident(&mut self) -> Result<String, ParseError> {
        match self.cur().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok(name)
            }
            Tok::Keyword(k) => {
                self.advance();
                Ok(k.to_string())
            }
            _ => self.err("expected property name"),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        self.last_paren = false;
        // Legacy octal numbers and octal/`\8`/`\9` string escapes are SyntaxErrors in strict mode.
        if self.strict && self.toks[self.pos].legacy_octal {
            return self.err("legacy octal literals are not allowed in strict mode");
        }
        match self.cur().clone() {
            Tok::Num(n) => {
                self.advance();
                Ok(Expr::Num(n))
            }
            Tok::BigInt(n) => {
                self.advance();
                Ok(Expr::BigInt(n))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(Rc::from(s.as_str())))
            }
            Tok::Template(parts) => {
                self.advance();
                self.build_template(parts)
            }
            Tok::Regex { body, flags } => {
                self.advance();
                // A regex literal is validated when parsed: an invalid pattern/flags is an early
                // (parse-phase) SyntaxError, not a runtime one.
                if let Err(msg) = crate::regex::Regex::new(&body, &flags) {
                    return self.err(msg);
                }
                Ok(Expr::Regex {
                    body: Rc::from(body.as_str()),
                    flags: Rc::from(flags.as_str()),
                })
            }
            Tok::Keyword("true") => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::Keyword("false") => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::Keyword("null") => {
                self.advance();
                Ok(Expr::Null)
            }
            Tok::Keyword("this") => {
                self.advance();
                Ok(Expr::This)
            }
            Tok::Keyword("function") => {
                let f = self.parse_function(false, true)?;
                Ok(Expr::Func(Rc::new(f)))
            }
            Tok::Keyword("class") | Tok::Punct("@") => {
                let c = self.parse_class()?;
                Ok(Expr::Class(Rc::new(c)))
            }
            Tok::Keyword("super") => {
                self.advance();
                // `super` is only valid as a SuperProperty (`super.x` / `super[e]`) or a SuperCall
                // (`super(...)`). A SuperProperty requires a method/field/static-block context; a bare
                // `super` (neither form) is always a SyntaxError. (SuperCall validity — a derived
                // constructor — is checked later.)
                if self.is_punct(".") || self.is_punct("[") {
                    if !self.super_prop_ok {
                        return self.err("'super' keyword unexpected here");
                    }
                } else if !self.is_punct("(") {
                    return self.err("'super' keyword unexpected here");
                }
                Ok(Expr::Super)
            }
            // `import(specifier)` (dynamic import), `import.meta`, or the phased forms
            // `import.source(specifier)` / `import.defer(specifier)`.
            Tok::Keyword("import") => {
                self.advance();
                if self.eat_punct(".") {
                    let phase = match self.cur() {
                        Tok::Ident(w) if w == "source" => ImportPhase::Source,
                        Tok::Ident(w) if w == "defer" => ImportPhase::Defer,
                        Tok::Ident(w) if w == "meta" => {
                            self.advance();
                            if !self.module {
                                return self.err("'import.meta' is only valid in a module");
                            }
                            return Ok(Expr::ImportMeta);
                        }
                        _ => {
                            return self
                                .err("expected 'meta', 'source', or 'defer' after 'import.'")
                        }
                    };
                    self.advance(); // 'source' / 'defer'
                    let spec = self.parse_import_call_args()?;
                    Ok(Expr::ImportCall {
                        spec: Box::new(spec),
                        phase,
                    })
                } else {
                    let spec = self.parse_import_call_args()?;
                    Ok(Expr::ImportCall {
                        spec: Box::new(spec),
                        phase: ImportPhase::Evaluation,
                    })
                }
            }
            Tok::Ident(name)
                if name == "async"
                    && !self.cur_escaped()
                    && matches!(self.peek_kind(1), Tok::Keyword("function")) =>
            {
                self.advance();
                let f = self.parse_function(true, true)?;
                Ok(Expr::Func(Rc::new(f)))
            }
            Tok::Ident(name) => {
                self.advance();
                // A strict-mode future-reserved word (yield, let, static, implements, …) cannot be
                // used as an identifier reference — but eval/arguments are only restricted as
                // assignment/binding targets, so they remain valid references.
                if self.strict && is_strict_reserved_word(&name) {
                    return self.err(format!("'{name}' is a reserved word in strict mode"));
                }
                // `yield` / `await` are reserved as identifier references inside a generator /
                // async context (even spelled with `\u` escapes).
                if name == "yield" && self.in_generator {
                    return self.err("'yield' is not a valid identifier in a generator");
                }
                if name == "await" && self.in_async {
                    return self.err("'await' is not a valid identifier here");
                }
                match name.as_str() {
                    "undefined" => Ok(Expr::Undefined),
                    _ => Ok(Expr::Ident(name)),
                }
            }
            Tok::Punct("(") => {
                self.advance();
                let e = self.parse_expr_allow_in()?;
                self.expect_punct(")")?;
                self.last_paren = true;
                Ok(e)
            }
            Tok::Punct("[") => self.parse_array(),
            Tok::Punct("{") => self.parse_object(),
            other => self.err(format!("unexpected token {other:?}")),
        }
    }

    /// Desugar a template literal into a string concatenation: cooked chunks become string
    /// literals, `${...}` holes are sub-parsed as expressions. Starting from a string literal makes
    /// every `+` a string concatenation (which ToString-coerces each substitution).
    fn build_template(&mut self, parts: Vec<TplPart>) -> Result<Expr, ParseError> {
        let mut expr: Option<Expr> = None;
        for part in parts {
            let piece = match part {
                TplPart::Str { cooked, .. } => Expr::Str(Rc::from(cooked.as_str())),
                TplPart::Sub(src) => {
                    let tokens = tokenize(&src).map_err(|e| ParseError {
                        message: e.message,
                        line: e.line,
                    })?;
                    let mut sub = Parser {
                        toks: tokens,
                        pos: 0,
                        strict: self.strict,
                        depth: self.depth,
                        in_generator: self.in_generator,
                        in_async: self.in_async,
                        in_params: self.in_params,
                        no_in: false,
                        module: self.module,
                        fn_depth: self.fn_depth,
                        iter_depth: self.iter_depth,
                        switch_depth: self.switch_depth,
                        labels: Vec::new(),
                        decl_scopes: vec![DeclScope {
                            fn_boundary: true,
                            ..Default::default()
                        }],
                        next_scope_is_fn_boundary: false,
                        allow_new_target: self.allow_new_target,
                        top_level: false,
                        super_prop_ok: self.super_prop_ok,
                        proto_dups: Vec::new(),
                        last_paren: false,
                        single_stmt: false,
                    };
                    // A substitution is ToString'd (string hint), not concatenated raw.
                    let e = sub.parse_expr()?;
                    self.proto_dups.append(&mut sub.proto_dups);
                    Expr::ToStr(Box::new(e))
                }
            };
            expr = Some(match expr {
                None => piece,
                Some(left) => Expr::Binary {
                    op: "+",
                    left: Box::new(left),
                    right: Box::new(piece),
                },
            });
        }
        Ok(expr.unwrap_or(Expr::Str(Rc::from(""))))
    }

    fn build_tagged_template(
        &mut self,
        tag: Expr,
        parts: Vec<TplPart>,
    ) -> Result<Expr, ParseError> {
        let mut quasis = Vec::new();
        let mut subs = Vec::new();
        for part in parts {
            match part {
                TplPart::Str { cooked, raw } => quasis.push((Some(cooked), raw)),
                TplPart::Sub(src) => {
                    let tokens = tokenize(&src).map_err(|e| ParseError {
                        message: e.message,
                        line: e.line,
                    })?;
                    let mut sub = Parser {
                        toks: tokens,
                        pos: 0,
                        strict: self.strict,
                        depth: self.depth,
                        in_generator: self.in_generator,
                        in_async: self.in_async,
                        in_params: self.in_params,
                        no_in: false,
                        module: self.module,
                        fn_depth: self.fn_depth,
                        iter_depth: self.iter_depth,
                        switch_depth: self.switch_depth,
                        labels: Vec::new(),
                        decl_scopes: vec![DeclScope {
                            fn_boundary: true,
                            ..Default::default()
                        }],
                        next_scope_is_fn_boundary: false,
                        allow_new_target: self.allow_new_target,
                        top_level: false,
                        super_prop_ok: self.super_prop_ok,
                        proto_dups: Vec::new(),
                        last_paren: false,
                        single_stmt: false,
                    };
                    let e = sub.parse_expr()?;
                    self.proto_dups.append(&mut sub.proto_dups);
                    subs.push(e);
                }
            }
        }
        Ok(Expr::TaggedTemplate {
            tag: Box::new(tag),
            quasis,
            subs,
        })
    }

    fn parse_array(&mut self) -> Result<Expr, ParseError> {
        self.expect_punct("[")?;
        let saved = self.no_in;
        self.no_in = false;
        let mut elems = Vec::new();
        while !self.is_punct("]") {
            if self.is_punct(",") {
                self.advance();
                elems.push(ArrayElem::Hole);
                continue;
            }
            if self.eat_punct("...") {
                elems.push(ArrayElem::Spread(self.parse_assign()?));
            } else {
                elems.push(ArrayElem::Item(self.parse_assign()?));
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_punct("]")?;
        Ok(Expr::Array(elems))
    }

    fn parse_object(&mut self) -> Result<Expr, ParseError> {
        self.expect_punct("{")?;
        let saved = self.no_in;
        self.no_in = false;
        let mut props = Vec::new();
        let mut proto_seen = false;
        while !self.is_punct("}") {
            if self.eat_punct("...") {
                props.push(PropDef::Spread(self.parse_assign()?));
                if !self.eat_punct(",") {
                    break;
                }
                continue;
            }
            // get/set accessors
            if (self.is_ident_word("get") || self.is_ident_word("set"))
                && !self.cur_escaped()
                && !matches!(
                    self.peek_kind(1),
                    Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("}") | Tok::Punct("(")
                )
            {
                let is_get = self.is_ident_word("get");
                self.advance();
                let key = self.parse_prop_key()?;
                let func = self.parse_accessor_function(is_get)?;
                props.push(if is_get {
                    PropDef::Getter {
                        key,
                        func: Rc::new(func),
                    }
                } else {
                    PropDef::Setter {
                        key,
                        func: Rc::new(func),
                    }
                });
                if !self.eat_punct(",") {
                    break;
                }
                continue;
            }
            // `async` / generator `*` method prefixes (async only when followed by a key start).
            let is_async = self.is_ident_word("async")
                && !self.cur_escaped()
                && !matches!(
                    self.peek_kind(1),
                    Tok::Punct(":")
                        | Tok::Punct(",")
                        | Tok::Punct("}")
                        | Tok::Punct("(")
                        | Tok::Punct("=")
                );
            if is_async {
                self.advance();
            }
            let is_generator = self.eat_punct("*");
            let key = self.parse_prop_key()?;
            if self.is_punct("(") {
                // Method shorthand.
                let func = if is_async || is_generator {
                    self.parse_method_function_kind(is_generator, is_async)?
                } else {
                    self.parse_method_function()?
                };
                // An object-literal method is never a derived constructor, so `super(...)` is illegal.
                if let Some(msg) = crate::eval::method_super_call_error_full(&func) {
                    return self.err(msg);
                }
                props.push(PropDef::Method {
                    key,
                    func: Rc::new(func),
                });
            } else if self.eat_punct(":") {
                // Two `__proto__: value` data properties in one literal are a SyntaxError — but
                // only if this stays a literal, so the error is deferred (see `proto_dups`): a
                // destructuring assignment pattern may repeat the key.
                let is_proto = matches!(&key, PropKey::Ident(n) if n == "__proto__")
                    || matches!(&key, PropKey::Str(s) if &**s == "__proto__");
                if is_proto {
                    if proto_seen {
                        self.proto_dups.push(ParseError {
                            message: "duplicate __proto__ property in object literal".into(),
                            line: self.line(),
                        });
                    }
                    proto_seen = true;
                }
                let value = self.parse_assign()?;
                // The colon-form `__proto__:` sets the prototype (as a value literal); a
                // destructuring pattern reinterprets it as a normal keyed target later.
                if is_proto {
                    props.push(PropDef::Proto(value));
                } else {
                    props.push(PropDef::KeyValue { key, value });
                }
            } else {
                // Shorthand `{ x }`, or CoverInitializedName `{ x = default }` (only meaningful in
                // a destructuring assignment target). Only valid when key is a plain identifier.
                match &key {
                    PropKey::Ident(name) => {
                        self.check_shorthand_ident(name)?;
                        let ident = Expr::Ident(name.clone());
                        let value = if self.eat_punct("=") {
                            let default = self.parse_assign()?;
                            Expr::Assign {
                                op: "=",
                                target: Box::new(ident),
                                value: Box::new(default),
                            }
                        } else {
                            ident
                        };
                        props.push(PropDef::KeyValue { key, value });
                    }
                    _ => return self.err("expected ':' after property key"),
                }
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_punct("}")?;
        Ok(Expr::Object(props))
    }

    fn parse_prop_key(&mut self) -> Result<PropKey, ParseError> {
        match self.cur().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok(PropKey::Ident(name))
            }
            Tok::Keyword(k) => {
                self.advance();
                Ok(PropKey::Ident(k.to_string()))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(PropKey::Str(Rc::from(s.as_str())))
            }
            Tok::Num(n) => {
                self.advance();
                Ok(PropKey::Num(n))
            }
            // A BigInt literal property key (`{1n: x}`) uses its integer string as the key.
            Tok::BigInt(n) => {
                self.advance();
                Ok(PropKey::Str(Rc::from(n.to_string().as_str())))
            }
            Tok::Punct("[") => {
                self.advance();
                // A computed key is `AssignmentExpression[+In]` — `in` is always allowed inside the
                // brackets, even in a `for` head's `[NoIn]` context.
                let e = self.parse_assign_allow_in()?;
                self.expect_punct("]")?;
                Ok(PropKey::Computed(e))
            }
            _ => self.err("expected property key"),
        }
    }

    // ----- functions --------------------------------------------------------------------------

    fn parse_function(&mut self, is_async: bool, is_expr: bool) -> Result<Function, ParseError> {
        self.eat_kw("function");
        let is_generator = self.eat_punct("*");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            self.advance();
            Some(n)
        } else {
            None
        };
        // A function *expression*'s own name binds inside the function, so a generator expression
        // cannot be named `yield` and an async one cannot be named `await`. (A declaration's name
        // binds in the enclosing scope and follows the enclosing context's rules instead.)
        if is_expr {
            if let Some(n) = &name {
                if (is_generator && n == "yield") || (is_async && n == "await") {
                    return self.err(format!("'{n}' cannot name this function expression"));
                }
            }
        }
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        // An ordinary function body is not a super-property context (a nested arrow would inherit
        // from here, correctly seeing no super).
        let ssuper = std::mem::replace(&mut self.super_prop_ok, false);
        let params = self.parse_params()?;
        let (body, is_strict) = self.parse_function_body(!params_complex(&params))?;
        self.super_prop_ok = ssuper;
        self.in_generator = sg;
        self.in_async = sa;
        let strict = is_strict || self.strict;
        // A function whose own body is strict ("use strict") also subjects its name and parameters
        // to the strict reserved-word rules (eval/arguments/yield/…), even in non-strict surroundings.
        if strict {
            if let Some(n) = &name {
                if is_strict_reserved_binding(n) {
                    return self.err(format!("'{n}' can't be a function name in strict mode"));
                }
            }
            for pn in param_names(&params) {
                if is_strict_reserved_binding(&pn) {
                    return self.err(format!("'{pn}' can't be a parameter name in strict mode"));
                }
            }
        }
        // Duplicate parameters are an error in strict mode, or whenever the list is non-simple
        // (defaults / rest / destructuring).
        if strict || params_complex(&params) {
            if let Some(dup) = duplicate_name(&param_names(&params)) {
                return self.err(format!("duplicate parameter name '{dup}'"));
            }
        }
        if let Some(dup) = params_body_lexical_clash(&params, &body) {
            return self.err(format!("Identifier '{dup}' has already been declared"));
        }
        let func = Function {
            name,
            params,
            body,
            is_arrow: false,
            is_strict: strict,
            expr_body: false,
            is_generator,
            is_async,
            is_method: false,
            is_fn_expr: is_expr,
        };
        // A function declaration/expression is never a derived constructor, so a `super(...)` call
        // in its body or parameters is an early SyntaxError.
        if let Some(msg) = crate::eval::method_super_call_error_full(&func) {
            return self.err(msg);
        }
        Ok(func)
    }

    // ----- classes ----------------------------------------------------------------------------

    /// Parse a run of `@decorator` prefixes (a class or class-element may carry several).
    fn parse_decorators(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut out = Vec::new();
        while self.is_punct("@") {
            self.advance();
            out.push(self.parse_decorator()?);
        }
        Ok(out)
    }

    /// A single decorator: `@( Expression )`, or a `DecoratorMemberExpression` (an identifier with a
    /// `.name`/`.#private` chain) optionally ending in one call `(...)`.
    fn parse_decorator(&mut self) -> Result<Expr, ParseError> {
        if self.is_punct("(") {
            self.advance();
            let e = self.parse_expr()?;
            self.expect_punct(")")?;
            return Ok(e);
        }
        let name = match self.cur().clone() {
            Tok::Ident(n) => {
                self.advance();
                n
            }
            Tok::Keyword(k) => {
                self.advance();
                k.to_string()
            }
            _ => return self.err("expected a decorator expression after '@'"),
        };
        let mut expr = Expr::Ident(name);
        while self.eat_punct(".") {
            let prop = self.parse_property_name_ident()?;
            expr = Expr::Member {
                obj: Box::new(expr),
                prop,
                optional: false,
            };
        }
        if self.is_punct("(") {
            let args = self.parse_args()?;
            expr = Expr::Call {
                callee: Box::new(expr),
                args,
                optional: false,
            };
        }
        Ok(expr)
    }

    fn parse_class(&mut self) -> Result<Class, ParseError> {
        let decorators = self.parse_decorators()?;
        self.eat_kw("class");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            self.advance();
            // A class definition is always strict, so its name can't be a reserved word.
            if is_strict_reserved_binding(&n) {
                return self.err(format!("'{n}' cannot be used as a class name"));
            }
            Some(n)
        } else {
            None
        };
        let superclass = if self.eat_kw("extends") {
            Some(Box::new(self.parse_lhs()?))
        } else {
            None
        };
        self.expect_punct("{")?;
        // Class bodies are always strict mode.
        let saved = self.strict;
        self.strict = true;
        let mut members = Vec::new();
        while !self.is_punct("}") && !self.at_eof() {
            members.extend(self.parse_class_member()?);
        }
        self.strict = saved;
        self.expect_punct("}")?;
        validate_class(&members).map_err(|m| ParseError {
            message: m,
            line: self.line(),
        })?;
        Ok(Class {
            name,
            superclass,
            members,
            decorators,
        })
    }

    /// True when the token `ahead` of the cursor ends a member head — i.e. the current contextual
    /// word (`static`/`get`/`set`/`async`) is actually the member *name*, not a modifier.
    fn next_is_member_terminator(&self, ahead: usize) -> bool {
        matches!(
            self.peek_kind(ahead),
            Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}")
        )
    }

    fn parse_class_member(&mut self) -> Result<Vec<ClassMember>, ParseError> {
        if self.eat_punct(";") {
            return Ok(vec![]);
        }
        let decorators = self.parse_decorators()?;
        let mut is_static = false;
        if self.is_ident_word("static") && !self.cur_escaped() && !self.next_is_member_terminator(1)
        {
            self.advance();
            is_static = true;
        }
        // `static { ... }` initialization block — a super-property context and function-like code
        // where `new.target` is valid (evaluates to undefined).
        if is_static && self.is_punct("{") {
            self.advance();
            let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
            let snt = std::mem::replace(&mut self.allow_new_target, true);
            let body = self.parse_block_body();
            self.super_prop_ok = ssuper;
            self.allow_new_target = snt;
            let body = body?;
            let func = Function {
                name: None,
                params: Vec::new(),
                body,
                is_arrow: false,
                is_strict: true,
                expr_body: false,
                is_generator: false,
                is_async: false,
                is_method: false,
                is_fn_expr: false,
            };
            return Ok(vec![ClassMember {
                key: PropKey::Ident(String::new()),
                kind: MemberKind::StaticBlock,
                is_static: true,
                func: Some(Rc::new(func)),
                value: None,
                decorators,
            }]);
        }
        // `accessor x = init;` (auto-accessor): desugar to a private backing field plus a getter and
        // setter that read/write it, reusing the ordinary class-element machinery.
        if self.is_ident_word("accessor") && !self.next_is_member_terminator(1) && !self.nl_at(1) {
            self.advance(); // `accessor`
            let key = self.parse_prop_key()?;
            let init = if self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            self.consume_semicolon()?;
            return Ok(vec![ClassMember {
                key,
                kind: MemberKind::Accessor,
                is_static,
                func: None,
                value: init,
                decorators,
            }]);
        }
        let mut kind = MemberKind::Method;
        if (self.is_ident_word("get") || self.is_ident_word("set"))
            && !self.cur_escaped()
            && !self.next_is_member_terminator(1)
        {
            kind = if self.is_ident_word("get") {
                MemberKind::Get
            } else {
                MemberKind::Set
            };
            self.advance();
        }
        let is_async = self.is_ident_word("async")
            && !self.cur_escaped()
            && !self.next_is_member_terminator(1);
        if is_async {
            self.advance();
        }
        let is_generator = self.eat_punct("*");

        let key = self.parse_prop_key()?;

        if self.is_punct("(") {
            let mut func = self.parse_method_function_kind(is_generator, is_async)?;
            if matches!(kind, MemberKind::Get | MemberKind::Set) {
                check_accessor_arity(&func, kind == MemberKind::Get).map_err(|m| ParseError {
                    message: m,
                    line: self.line(),
                })?;
            }
            let kind = if kind == MemberKind::Method && !is_static && key_is(&key, "constructor") {
                // The class `constructor` is the class's [[Construct]] — not a mere method.
                func.is_method = false;
                MemberKind::Constructor
            } else {
                kind
            };
            Ok(vec![ClassMember {
                key,
                kind,
                is_static,
                func: Some(Rc::new(func)),
                value: None,
                decorators,
            }])
        } else {
            // Field declaration. A field initializer is a super-property context and function-like
            // code where `new.target` is valid (it evaluates to undefined).
            let value = if self.eat_punct("=") {
                let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
                let snt = std::mem::replace(&mut self.allow_new_target, true);
                let v = self.parse_assign();
                self.super_prop_ok = ssuper;
                self.allow_new_target = snt;
                Some(v?)
            } else {
                None
            };
            self.consume_semicolon()?;
            Ok(vec![ClassMember {
                key,
                kind: MemberKind::Field,
                is_static,
                func: None,
                value,
                decorators,
            }])
        }
    }

    fn parse_method_function(&mut self) -> Result<Function, ParseError> {
        self.parse_method_function_kind(false, false)
    }

    fn parse_method_function_kind(
        &mut self,
        is_generator: bool,
        is_async: bool,
    ) -> Result<Function, ParseError> {
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        // A method body (and any parameter default) is a super-property context.
        let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
        let params = self.parse_params()?;
        // A method has UniqueFormalParameters: duplicate parameter names are always an error.
        if let Some(dup) = duplicate_name(&param_names(&params)) {
            self.in_generator = sg;
            self.in_async = sa;
            self.super_prop_ok = ssuper;
            return self.err(format!("duplicate parameter name '{dup}'"));
        }
        let (body, is_strict) = self.parse_function_body(!params_complex(&params))?;
        self.super_prop_ok = ssuper;
        self.in_generator = sg;
        self.in_async = sa;
        if let Some(dup) = params_body_lexical_clash(&params, &body) {
            return self.err(format!("Identifier '{dup}' has already been declared"));
        }
        Ok(Function {
            name: None,
            params,
            body,
            is_arrow: false,
            is_strict: is_strict || self.strict,
            expr_body: false,
            is_generator,
            is_async,
            is_method: true,
            is_fn_expr: false,
        })
    }

    fn parse_accessor_function(&mut self, is_get: bool) -> Result<Function, ParseError> {
        let f = self.parse_method_function()?;
        check_accessor_arity(&f, is_get).map_err(|m| ParseError {
            message: m,
            line: self.line(),
        })?;
        Ok(f)
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        self.expect_punct("(")?;
        let saved_in_params = self.in_params;
        self.in_params = true;
        let result = self.parse_params_inner();
        self.in_params = saved_in_params;
        result
    }

    fn parse_params_inner(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut params = Vec::new();
        while !self.is_punct(")") {
            let rest = self.eat_punct("...");
            let pattern = self.parse_binding_pattern()?;
            let default = if !rest && self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            params.push(Param {
                pattern,
                default,
                rest,
            });
            if rest || !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(params)
    }

    fn parse_function_body(
        &mut self,
        params_simple: bool,
    ) -> Result<(Vec<Stmt>, bool), ParseError> {
        self.expect_punct("{")?;
        let saved_strict = self.strict;
        let inner_strict = matches!(self.cur(), Tok::Str(s) if s == "use strict");
        // A function with a non-simple parameter list (defaults / rest / destructuring) can't apply a
        // `"use strict"` directive to its own body.
        if inner_strict && !params_simple {
            return self.err(
                "illegal 'use strict' directive in a function with a non-simple parameter list",
            );
        }
        if inner_strict {
            self.strict = true;
        }
        // A function body is a fresh context for return/break/continue and labels.
        let (siter, sswitch) = (self.iter_depth, self.switch_depth);
        let slabels = std::mem::take(&mut self.labels);
        let saved_in_params = self.in_params;
        self.in_params = false;
        self.fn_depth += 1;
        self.iter_depth = 0;
        self.switch_depth = 0;
        self.next_scope_is_fn_boundary = true;
        let body = self.parse_block_body();
        self.fn_depth -= 1;
        self.iter_depth = siter;
        self.switch_depth = sswitch;
        self.labels = slabels;
        self.in_params = saved_in_params;
        let body = body?;
        let result_strict = self.strict;
        self.strict = saved_strict;
        Ok((body, result_strict))
    }

    // ----- arrow functions --------------------------------------------------------------------

    fn try_parse_arrow(&mut self) -> Result<Option<Expr>, ParseError> {
        // Optional `async` prefix (on the same line) for an async arrow.
        let async_arrow = self.is_ident_word("async")
            && !self
                .toks
                .get(self.pos + 1)
                .map(|t| t.nl_before)
                .unwrap_or(true)
            && matches!(self.peek_kind(1), Tok::Ident(_) | Tok::Punct("("));
        let base = if async_arrow { 1 } else { 0 };

        // `ident => ...`
        if let Tok::Ident(name) = self.peek_kind(base) {
            if matches!(self.peek_kind(base + 1), Tok::Punct("=>"))
                && !self.toks[self.pos + base + 1].nl_before
            {
                for _ in 0..=base {
                    self.advance(); // (async) ident
                }
                self.advance(); // =>
                let params = vec![Param {
                    pattern: Pattern::Ident(name),
                    default: None,
                    rest: false,
                }];
                return Ok(Some(self.finish_arrow(params, async_arrow)?));
            }
        }
        // `( params ) => ...`
        if matches!(self.peek_kind(base), Tok::Punct("(")) {
            if let Some(close) = self.matching_paren(self.pos + base) {
                if matches!(
                    self.toks.get(close + 1).map(|t| &t.kind),
                    Some(Tok::Punct("=>"))
                ) && !self.toks[close + 1].nl_before
                {
                    if async_arrow {
                        self.advance();
                    }
                    let params = self.parse_params()?;
                    self.expect_punct("=>")?;
                    return Ok(Some(self.finish_arrow(params, async_arrow)?));
                }
            }
        }
        Ok(None)
    }

    fn finish_arrow(&mut self, params: Vec<Param>, is_async: bool) -> Result<Expr, ParseError> {
        // Arrow parameters must always be unique (the list is treated as if it had a `[+Strict]`).
        if let Some(dup) = duplicate_name(&param_names(&params)) {
            return self.err(format!("duplicate parameter name '{dup}'"));
        }
        let sa = self.in_async;
        self.in_async = is_async;
        let result = if self.is_punct("{") {
            let (body, is_strict) = self.parse_function_body(!params_complex(&params))?;
            Function {
                name: None,
                params,
                body,
                is_arrow: true,
                is_strict: is_strict || self.strict,
                expr_body: false,
                is_generator: false,
                is_async,
                is_method: false,
                is_fn_expr: false,
            }
        } else {
            let expr = self.parse_assign()?;
            Function {
                name: None,
                params,
                body: vec![Stmt::Return(Some(expr))],
                is_arrow: true,
                is_strict: self.strict,
                expr_body: true,
                is_generator: false,
                is_async,
                is_method: false,
                is_fn_expr: false,
            }
        };
        self.in_async = sa;
        Ok(Expr::Func(Rc::new(result)))
    }

    /// Index of the `)` matching the `(` at `open`, scanning balanced brackets.
    fn matching_paren(&self, open: usize) -> Option<usize> {
        let mut depth = 0i32;
        let mut i = open;
        while i < self.toks.len() {
            match &self.toks[i].kind {
                Tok::Punct("(") | Tok::Punct("[") | Tok::Punct("{") => depth += 1,
                Tok::Punct(")") | Tok::Punct("]") | Tok::Punct("}") => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                Tok::Eof => return None,
                _ => {}
            }
            i += 1;
        }
        None
    }
}

fn is_assign_op(op: &str) -> bool {
    matches!(
        op,
        "=" | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "**="
            | "<<="
            | ">>="
            | ">>>="
            | "&="
            | "|="
            | "^="
            | "&&="
            | "||="
            | "??="
    )
}

fn is_valid_assign_target(e: &Expr) -> bool {
    matches!(e, Expr::Ident(_) | Expr::Member { .. } | Expr::Index { .. })
}

/// Whether an array/object literal is a valid *destructuring assignment* pattern. Unlike a binding
/// pattern (see `expr_to_pattern`), an object `AssignmentRestProperty` target may be any
/// `LeftHandSideExpression` that is not itself a nested pattern (e.g. a member expression), and
/// element/property targets may be member expressions too.
fn is_valid_assign_pattern(e: &Expr) -> bool {
    match e {
        Expr::Ident(_) => true,
        Expr::Member {
            optional: false, ..
        }
        | Expr::Index {
            optional: false, ..
        } => true,
        Expr::Array(elems) => elems.iter().enumerate().all(|(idx, el)| match el {
            ArrayElem::Hole => true,
            ArrayElem::Spread(t) => {
                idx == elems.len() - 1
                    && !matches!(t, Expr::Assign { op: "=", .. })
                    && is_valid_assign_pattern(t)
            }
            ArrayElem::Item(Expr::Assign {
                op: "=", target, ..
            }) => is_valid_assign_pattern(target),
            ArrayElem::Item(t) => is_valid_assign_pattern(t),
        }),
        Expr::Object(props) => props.iter().enumerate().all(|(idx, p)| match p {
            PropDef::KeyValue {
                value: Expr::Assign {
                    op: "=", target, ..
                },
                ..
            } => is_valid_assign_pattern(target),
            PropDef::KeyValue { value, .. } => is_valid_assign_pattern(value),
            // `__proto__: t` as a pattern is a plain keyed target (`t` may itself carry a default).
            PropDef::Proto(Expr::Assign {
                op: "=", target, ..
            }) => is_valid_assign_pattern(target),
            PropDef::Proto(value) => is_valid_assign_pattern(value),
            // Rest must be last and its target must not be a nested pattern.
            PropDef::Spread(t) => {
                idx == props.len() - 1
                    && !matches!(t, Expr::Array(_) | Expr::Object(_))
                    && is_valid_assign_pattern(t)
            }
            _ => false,
        }),
        _ => false,
    }
}

fn key_is(key: &PropKey, name: &str) -> bool {
    match key {
        PropKey::Ident(s) => s == name,
        PropKey::Str(s) => &**s == name,
        _ => false,
    }
}

/// Class early errors: at most one constructor, no `#constructor`, and each private name is declared
/// once (a get/set accessor pair for the same name being the only exception).
fn validate_class(members: &[ClassMember]) -> Result<(), String> {
    let mut ctor_count = 0;
    // private name → the (kind, is_static) of each declaration under it.
    let mut private: Vec<(String, Vec<(MemberKind, bool)>)> = Vec::new();
    for m in members {
        if matches!(m.kind, MemberKind::Constructor) {
            ctor_count += 1;
            if ctor_count > 1 {
                return Err("a class may only have one constructor".into());
            }
        }
        // A non-constructor method body may not contain a `super(...)` call.
        if let Some(func) = &m.func {
            if !matches!(m.kind, MemberKind::Constructor) {
                if let Some(msg) = crate::eval::method_super_call_error_full(func) {
                    return Err(msg.into());
                }
            }
            // The constructor can't be a generator, async, getter, or setter.
            if !m.is_static
                && key_is(&m.key, "constructor")
                && (matches!(m.kind, MemberKind::Get | MemberKind::Set)
                    || func.is_generator
                    || func.is_async)
            {
                return Err("class constructor can't be a generator, async, or accessor".into());
            }
            // A static method (any kind) may not be named "prototype".
            if m.is_static && key_is(&m.key, "prototype") {
                return Err("classes may not have a static method named 'prototype'".into());
            }
        }
        // Field name rules: no field may be named "constructor"; a static field can't be "prototype".
        if matches!(m.kind, MemberKind::Field) {
            if key_is(&m.key, "constructor") {
                return Err("classes may not have a field named 'constructor'".into());
            }
            if m.is_static && key_is(&m.key, "prototype") {
                return Err("classes may not have a static field named 'prototype'".into());
            }
            // A field's computed name and its initializer may not use `arguments` or `super()`.
            if let PropKey::Computed(e) = &m.key {
                if let Some(msg) = crate::eval::field_init_error(e) {
                    return Err(msg.into());
                }
            }
            if let Some(v) = &m.value {
                if let Some(msg) = crate::eval::field_init_error(v) {
                    return Err(msg.into());
                }
            }
        }
        // A `static { … }` block may not contain `arguments` (nor a `super(...)` call).
        if matches!(m.kind, MemberKind::StaticBlock) {
            if let Some(func) = &m.func {
                if let Some(msg) = crate::eval::static_block_error(func) {
                    return Err(msg.into());
                }
            }
        }
        if let PropKey::Ident(name) = &m.key {
            if name.starts_with('#') {
                if name == "#constructor" {
                    return Err("'#constructor' is a reserved private name".into());
                }
                // A private name occupies one slot for the whole class, regardless of static-ness.
                let entry = private.iter_mut().find(|(n, _)| n == name);
                match entry {
                    Some((_, kinds)) => kinds.push((m.kind, m.is_static)),
                    None => private.push((name.clone(), vec![(m.kind, m.is_static)])),
                }
            }
        }
    }
    for (name, kinds) in &private {
        // Valid: a single declaration, or a get/set accessor pair with matching static-ness.
        let ok = kinds.len() == 1
            || (kinds.len() == 2
                && kinds[0].1 == kinds[1].1
                && kinds.iter().any(|(k, _)| matches!(k, MemberKind::Get))
                && kinds.iter().any(|(k, _)| matches!(k, MemberKind::Set)));
        if !ok {
            return Err(format!("duplicate private name '{name}'"));
        }
    }
    Ok(())
}

/// Every referenced private name (`obj.#x`, `#x in obj`) must be declared in the class it appears in
/// or an enclosing one. Walks the AST with a stack of each class's declared private names; entering a
/// class pushes its names (so forward references within the class resolve) and leaving pops them.
fn validate_private_names(stmts: &[Stmt]) -> Result<(), String> {
    let mut st: Vec<Vec<String>> = Vec::new();
    pn_stmts(stmts, &mut st)
}

fn pn_declared(st: &[Vec<String>], name: &str) -> bool {
    st.iter().any(|s| s.iter().any(|n| n == name))
}

fn pn_class(class: &Class, st: &mut Vec<Vec<String>>) -> Result<(), String> {
    if let Some(s) = &class.superclass {
        pn_expr(s, st)?;
    }
    let names = class
        .members
        .iter()
        .filter_map(|m| match &m.key {
            PropKey::Ident(n) if n.starts_with('#') => Some(n.clone()),
            _ => None,
        })
        .collect();
    st.push(names);
    for m in &class.members {
        if let PropKey::Computed(e) = &m.key {
            pn_expr(e, st)?;
        }
        if let Some(f) = &m.func {
            pn_params(&f.params, st)?;
            pn_stmts(&f.body, st)?;
        }
        if let Some(v) = &m.value {
            pn_expr(v, st)?;
        }
    }
    st.pop();
    Ok(())
}

fn pn_params(params: &[Param], st: &mut Vec<Vec<String>>) -> Result<(), String> {
    for p in params {
        pn_pattern(&p.pattern, st)?;
        if let Some(d) = &p.default {
            pn_expr(d, st)?;
        }
    }
    Ok(())
}

fn pn_pattern(pat: &Pattern, st: &mut Vec<Vec<String>>) -> Result<(), String> {
    match pat {
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Elem { pattern, default } => {
                        pn_pattern(pattern, st)?;
                        if let Some(d) = default {
                            pn_expr(d, st)?;
                        }
                    }
                    ArrayPatElem::Rest(p) => pn_pattern(p, st)?,
                    ArrayPatElem::Hole => {}
                }
            }
        }
        Pattern::Object(o) => {
            for p in &o.props {
                pn_pattern(&p.value, st)?;
                if let Some(d) = &p.default {
                    pn_expr(d, st)?;
                }
            }
        }
        Pattern::Member(e) => pn_expr(e, st)?,
        Pattern::Ident(_) => {}
    }
    Ok(())
}

fn pn_stmts(stmts: &[Stmt], st: &mut Vec<Vec<String>>) -> Result<(), String> {
    for s in stmts {
        pn_stmt(s, st)?;
    }
    Ok(())
}

fn pn_stmt(stmt: &Stmt, st: &mut Vec<Vec<String>>) -> Result<(), String> {
    match stmt {
        Stmt::Expr(e) | Stmt::Throw(e) => pn_expr(e, st)?,
        Stmt::VarDecl { decls, .. } => {
            for (pat, init) in decls {
                pn_pattern(pat, st)?;
                if let Some(e) = init {
                    pn_expr(e, st)?;
                }
            }
        }
        Stmt::FuncDecl(f) => {
            pn_params(&f.params, st)?;
            pn_stmts(&f.body, st)?;
        }
        Stmt::Return(Some(e)) => pn_expr(e, st)?,
        Stmt::If { test, cons, alt } => {
            pn_expr(test, st)?;
            pn_stmt(cons, st)?;
            if let Some(a) = alt {
                pn_stmt(a, st)?;
            }
        }
        Stmt::Block(b) => pn_stmts(b, st)?,
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            pn_expr(test, st)?;
            pn_stmt(body, st)?;
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            if let Some(fi) = init {
                match &**fi {
                    ForInit::VarDecl { decls, .. } => {
                        for (pat, e) in decls {
                            pn_pattern(pat, st)?;
                            if let Some(x) = e {
                                pn_expr(x, st)?;
                            }
                        }
                    }
                    ForInit::Expr(e) => pn_expr(e, st)?,
                }
            }
            if let Some(e) = test {
                pn_expr(e, st)?;
            }
            if let Some(e) = update {
                pn_expr(e, st)?;
            }
            pn_stmt(body, st)?;
        }
        Stmt::ForInOf {
            left, right, body, ..
        } => {
            pn_pattern(left, st)?;
            pn_expr(right, st)?;
            pn_stmt(body, st)?;
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            pn_stmts(block, st)?;
            if let Some((param, body)) = handler {
                if let Some(p) = param {
                    pn_pattern(p, st)?;
                }
                pn_stmts(body, st)?;
            }
            if let Some(f) = finalizer {
                pn_stmts(f, st)?;
            }
        }
        Stmt::Switch { disc, cases } => {
            pn_expr(disc, st)?;
            for c in cases {
                if let Some(t) = &c.test {
                    pn_expr(t, st)?;
                }
                pn_stmts(&c.body, st)?;
            }
        }
        Stmt::Labeled { body, .. } => pn_stmt(body, st)?,
        Stmt::With { obj, body } => {
            pn_expr(obj, st)?;
            pn_stmt(body, st)?;
        }
        Stmt::ClassDecl(c) => pn_class(c, st)?,
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => pn_stmt(inner, st)?,
        _ => {}
    }
    Ok(())
}

fn pn_args(args: &[ArrayElem], st: &mut Vec<Vec<String>>) -> Result<(), String> {
    for a in args {
        match a {
            ArrayElem::Item(e) | ArrayElem::Spread(e) => pn_expr(e, st)?,
            ArrayElem::Hole => {}
        }
    }
    Ok(())
}

fn pn_expr(expr: &Expr, st: &mut Vec<Vec<String>>) -> Result<(), String> {
    match expr {
        Expr::Member { obj, prop, .. } => {
            if prop.starts_with('#') && !pn_declared(st, prop) {
                return Err(format!(
                    "Private name '{prop}' is not declared in an enclosing class"
                ));
            }
            pn_expr(obj, st)?;
        }
        Expr::PrivateIn { name, obj } => {
            if !pn_declared(st, name) {
                return Err(format!(
                    "Private name '{name}' is not declared in an enclosing class"
                ));
            }
            pn_expr(obj, st)?;
        }
        Expr::Index { obj, index, .. } => {
            pn_expr(obj, st)?;
            pn_expr(index, st)?;
        }
        Expr::Class(c) => pn_class(c, st)?,
        Expr::Func(f) => {
            pn_params(&f.params, st)?;
            pn_stmts(&f.body, st)?;
        }
        Expr::Unary { arg, .. }
        | Expr::Update { arg, .. }
        | Expr::Await(arg)
        | Expr::OptionalChain(arg) => pn_expr(arg, st)?,
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            pn_expr(left, st)?;
            pn_expr(right, st)?;
        }
        Expr::Assign { target, value, .. } => {
            pn_expr(target, st)?;
            pn_expr(value, st)?;
        }
        Expr::Cond { test, cons, alt } => {
            pn_expr(test, st)?;
            pn_expr(cons, st)?;
            pn_expr(alt, st)?;
        }
        Expr::Call { callee, args, .. } => {
            pn_expr(callee, st)?;
            pn_args(args, st)?;
        }
        Expr::New { callee, args } => {
            pn_expr(callee, st)?;
            pn_args(args, st)?;
        }
        Expr::Array(elems) => pn_args(elems, st)?,
        Expr::Object(props) => {
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } => {
                        if let PropKey::Computed(e) = key {
                            pn_expr(e, st)?;
                        }
                        pn_expr(value, st)?;
                    }
                    PropDef::Method { key, func }
                    | PropDef::Getter { key, func }
                    | PropDef::Setter { key, func } => {
                        if let PropKey::Computed(e) = key {
                            pn_expr(e, st)?;
                        }
                        pn_params(&func.params, st)?;
                        pn_stmts(&func.body, st)?;
                    }
                    PropDef::Spread(e) | PropDef::Proto(e) => pn_expr(e, st)?,
                }
            }
        }
        Expr::Seq(v) => {
            for e in v {
                pn_expr(e, st)?;
            }
        }
        Expr::Yield { arg: Some(a), .. } => pn_expr(a, st)?,
        Expr::TaggedTemplate { tag, subs, .. } => {
            pn_expr(tag, st)?;
            for s in subs {
                pn_expr(s, st)?;
            }
        }
        Expr::ImportCall { spec, .. } => pn_expr(spec, st)?,
        _ => {}
    }
    Ok(())
}

/// Whether `new`'s callee chain bottoms out in an `import(...)` call (peeling member/index access),
/// which makes it a CallExpression and thus an invalid `new` operand.
fn callee_has_import_call(e: &Expr) -> bool {
    match e {
        Expr::ImportCall { .. } => true,
        Expr::Member { obj, .. } | Expr::Index { obj, .. } => callee_has_import_call(obj),
        _ => false,
    }
}

/// Whether `delete <arg>` targets a private member (`obj.#x`), which is a SyntaxError.
fn deletes_private_member(arg: &Expr) -> bool {
    match arg {
        Expr::Member { prop, .. } => prop.starts_with('#'),
        Expr::OptionalChain(inner) => deletes_private_member(inner),
        _ => false,
    }
}

/// A getter takes no parameters; a setter takes exactly one (non-rest) parameter.
fn check_accessor_arity(f: &Function, is_get: bool) -> Result<(), String> {
    if is_get {
        if !f.params.is_empty() {
            return Err("getter functions must have no arguments".into());
        }
    } else if f.params.len() != 1 || f.params[0].rest {
        return Err("setter functions must have exactly one argument".into());
    }
    Ok(())
}

fn pattern_names(pat: &Pattern, out: &mut Vec<String>) {
    match pat {
        Pattern::Ident(n) => out.push(n.clone()),
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Elem { pattern, .. } => pattern_names(pattern, out),
                    ArrayPatElem::Rest(p) => pattern_names(p, out),
                    ArrayPatElem::Hole => {}
                }
            }
        }
        Pattern::Object(o) => {
            for p in &o.props {
                pattern_names(&p.value, out);
            }
            if let Some(r) = &o.rest {
                out.push(r.clone());
            }
        }
        Pattern::Member(_) => {} // an assignment target binds no new names
    }
}
fn param_names(params: &[Param]) -> Vec<String> {
    let mut out = Vec::new();
    for p in params {
        pattern_names(&p.pattern, &mut out);
    }
    out
}
fn params_complex(params: &[Param]) -> bool {
    params
        .iter()
        .any(|p| p.default.is_some() || p.rest || !matches!(p.pattern, Pattern::Ident(_)))
}
fn duplicate_name(names: &[String]) -> Option<String> {
    for (idx, n) in names.iter().enumerate() {
        if names[..idx].contains(n) {
            return Some(n.clone());
        }
    }
    None
}

/// Future-reserved words that may not appear as an identifier *reference* in strict mode (unlike
/// eval/arguments, which are only restricted as assignment/binding targets).
fn is_strict_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
    )
}

/// Identifiers that may not be bound (or assigned) in strict mode.
fn is_strict_reserved_binding(name: &str) -> bool {
    matches!(
        name,
        "eval"
            | "arguments"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
    )
}

fn expr_to_pattern(e: &Expr) -> Option<Pattern> {
    match e {
        Expr::Ident(name) => Some(Pattern::Ident(name.clone())),
        Expr::Array(elems) => {
            let mut out = Vec::new();
            for (idx, el) in elems.iter().enumerate() {
                match el {
                    ArrayElem::Hole => out.push(ArrayPatElem::Hole),
                    ArrayElem::Spread(t) => {
                        // A rest element must be last and can't carry a default.
                        if idx != elems.len() - 1 || matches!(t, Expr::Assign { op: "=", .. }) {
                            return None;
                        }
                        out.push(ArrayPatElem::Rest(expr_to_pattern(t)?));
                    }
                    ArrayElem::Item(Expr::Assign {
                        op: "=",
                        target,
                        value,
                    }) => out.push(ArrayPatElem::Elem {
                        pattern: expr_to_pattern(target)?,
                        default: Some((**value).clone()),
                    }),
                    ArrayElem::Item(t) => out.push(ArrayPatElem::Elem {
                        pattern: expr_to_pattern(t)?,
                        default: None,
                    }),
                }
            }
            Some(Pattern::Array(out))
        }
        Expr::Object(props) => {
            let mut pat = ObjectPat {
                props: Vec::new(),
                rest: None,
            };
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } => {
                        let (value, default) = match value {
                            Expr::Assign {
                                op: "=",
                                target,
                                value: d,
                            } => (expr_to_pattern(target)?, Some((**d).clone())),
                            v => (expr_to_pattern(v)?, None),
                        };
                        pat.props.push(ObjPatProp {
                            key: key.clone(),
                            value,
                            default,
                        });
                    }
                    // As a pattern, `__proto__: target` is a plain keyed property named "__proto__".
                    PropDef::Proto(value) => {
                        let (value, default) = match value {
                            Expr::Assign {
                                op: "=",
                                target,
                                value: d,
                            } => (expr_to_pattern(target)?, Some((**d).clone())),
                            v => (expr_to_pattern(v)?, None),
                        };
                        pat.props.push(ObjPatProp {
                            key: PropKey::Ident("__proto__".to_string()),
                            value,
                            default,
                        });
                    }
                    PropDef::Spread(Expr::Ident(name)) => pat.rest = Some(name.clone()),
                    _ => return None,
                }
            }
            Some(Pattern::Object(pat))
        }
        // A member expression (`o.p` / `o[k]`) is a valid assignment target.
        Expr::Member {
            optional: false, ..
        }
        | Expr::Index {
            optional: false, ..
        } => Some(Pattern::Member(Box::new(e.clone()))),
        _ => None,
    }
}
