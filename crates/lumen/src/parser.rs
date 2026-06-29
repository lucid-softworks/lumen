//! Recursive-descent parser with Pratt precedence for binary operators. Produces the [`crate::ast`]
//! tree. Any failure is a [`ParseError`], which the engine reports as a SyntaxError (parse phase).

use crate::ast::*;
use crate::lexer::tokenize;
use crate::token::{Tok, Token, TplPart};
use std::rc::Rc;

pub struct ParseError {
    pub message: String,
    pub line: u32,
}

/// Parse a complete script. `strict` seeds strict mode (e.g. for the strict test262 variant); a
/// `"use strict"` directive prologue also turns it on.
pub fn parse_script(src: &str, strict: bool) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(src).map_err(|e| ParseError { message: e.message, line: e.line })?;
    let mut p = Parser { toks: tokens, pos: 0, strict, depth: 0, in_generator: false, in_async: false, no_in: false, module: false, fn_depth: 0, iter_depth: 0, switch_depth: 0, labels: Vec::new(), decl_scopes: vec![DeclScope::default()] };
    let strict_prologue = p.has_use_strict_prologue();
    p.strict = p.strict || strict_prologue;
    let body = p.parse_stmts_until_eof()?;
    Ok(body)
}

/// Parse a module (always strict; `import`/`export` are allowed only here). Modules permit top-level
/// `await`, so `await` is treated as a keyword at the module's top level.
pub fn parse_module(src: &str) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(src).map_err(|e| ParseError { message: e.message, line: e.line })?;
    let mut p = Parser { toks: tokens, pos: 0, strict: true, depth: 0, in_generator: false, in_async: true, no_in: false, module: true, fn_depth: 0, iter_depth: 0, switch_depth: 0, labels: Vec::new(), decl_scopes: vec![DeclScope::default()] };
    let body = p.parse_stmts_until_eof()?;
    validate_module(&body)?;
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
            Stmt::ExportAll { exported: Some(n), .. } => exported.push(n.clone()),
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
        return Err(ParseError { message: format!("duplicate export name '{dup}'"), line: 0 });
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
        Stmt::VarDecl { kind: DeclKind::Var, decls } => {
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
}

#[derive(Default)]
struct DeclScope {
    lexical: Vec<String>,
    var: Vec<String>,
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
        Err(ParseError { message: msg.into(), line: self.line() })
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

    fn push_decl_scope(&mut self) {
        self.decl_scopes.push(DeclScope::default());
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
        self.decl_scopes.last_mut().unwrap().lexical.push(name.to_string());
        Ok(())
    }
    /// Record a `var`/function binding; only conflicts with a lexical binding in the same scope.
    fn declare_var(&mut self, name: &str) -> Result<(), ParseError> {
        let conflict = self.decl_scopes.last().unwrap().lexical.iter().any(|n| n == name);
        if conflict {
            return self.err(format!("Identifier '{name}' has already been declared"));
        }
        self.decl_scopes.last_mut().unwrap().var.push(name.to_string());
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
            out.push(self.parse_stmt()?);
        }
        Ok(out)
    }

    // ----- statements -------------------------------------------------------------------------

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
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
            Tok::Ident(w) if w == "let" && self.starts_let_decl() => {
                self.parse_var_decl(DeclKind::Let)
            }
            Tok::Keyword("function") => {
                let f = self.parse_function(false)?;
                if let Some(n) = &f.name {
                    self.declare_var(n)?;
                }
                Ok(Stmt::FuncDecl(Rc::new(f)))
            }
            // `async function f(){}` declaration (async is a contextual keyword).
            Tok::Ident(w) if w == "async" && matches!(self.peek_kind(1), Tok::Keyword("function")) => {
                self.advance();
                let f = self.parse_function(true)?;
                if let Some(n) = &f.name {
                    self.declare_var(n)?;
                }
                Ok(Stmt::FuncDecl(Rc::new(f)))
            }
            Tok::Keyword("class") => {
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
                let arg = if self.can_end_stmt() { None } else { Some(self.parse_expr()?) };
                self.consume_semicolon()?;
                Ok(Stmt::Return(arg))
            }
            Tok::Keyword("break") => {
                self.advance();
                let label = self.parse_opt_label();
                match &label {
                    Some(l) if !self.labels.contains(l) => return self.err("undefined break label"),
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
                    Some(l) if !self.labels.contains(l) => return self.err("undefined continue label"),
                    None if self.iter_depth == 0 => return self.err("illegal 'continue' statement"),
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
                if self.module && !matches!(self.peek_kind(1), Tok::Punct("(") | Tok::Punct(".")) =>
            {
                self.parse_import()
            }
            Tok::Keyword("export") if self.module => self.parse_export(),
            Tok::Keyword("export") => self.err("'export' is only valid in a module"),
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
                let body = self.parse_substatement();
                self.labels.pop();
                Ok(Stmt::Labeled { label: name, body: Box::new(body?) })
            }
            _ => {
                let e = self.parse_expr()?;
                self.consume_semicolon()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn peek_kind(&self, ahead: usize) -> Tok {
        self.toks.get(self.pos + ahead).map(|t| t.kind.clone()).unwrap_or(Tok::Eof)
    }

    /// After `let`, decide whether this is a `let` declaration (vs `let` used as an identifier).
    fn starts_let_decl(&self) -> bool {
        matches!(self.peek_kind(1), Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{"))
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
        self.advance(); // var/let/const keyword (or `let` ident)
        let decls = self.parse_var_declarators()?;
        // A `const` declaration must have an initializer for each binding.
        if kind == DeclKind::Const && decls.iter().any(|(_, init)| init.is_none()) {
            return self.err("missing initializer in const declaration");
        }
        // Track declared names to catch lexical redeclaration.
        let mut names = Vec::new();
        for (pat, _) in &decls {
            pattern_names(pat, &mut names);
        }
        for n in &names {
            match kind {
                DeclKind::Var => self.declare_var(n)?,
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
            let init = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
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
                    return self.err(format!("'{name}' cannot be used as a binding in strict mode"));
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
            let default = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
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
                let d = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
                (v, d)
            } else {
                // Shorthand `{ a }` or `{ a = default }` — key must be a plain identifier.
                let name = match &key {
                    PropKey::Ident(n) => n.clone(),
                    _ => return self.err("invalid shorthand destructuring target"),
                };
                let d = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
                (Pattern::Ident(name), d)
            };
            props.push(ObjPatProp { key, value, default });
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
        let cons = Box::new(self.parse_substatement()?);
        let alt =
            if self.eat_kw("else") { Some(Box::new(self.parse_substatement()?)) } else { None };
        Ok(Stmt::If { test, cons, alt })
    }

    /// Parse a statement that is the body of `if`/loop/`with`/label — a lexical (`let`/`const`) or
    /// `class` declaration is not allowed in that position.
    fn parse_substatement(&mut self) -> Result<Stmt, ParseError> {
        let s = self.parse_stmt()?;
        if matches!(
            s,
            Stmt::VarDecl { kind: DeclKind::Let | DeclKind::Const, .. } | Stmt::ClassDecl(_)
        ) {
            return self.err("lexical declaration cannot appear in a single-statement context");
        }
        Ok(s)
    }

    /// Parse a loop body inside an iteration context (so `break`/`continue` are legal).
    fn parse_loop_body(&mut self) -> Result<Stmt, ParseError> {
        self.iter_depth += 1;
        let r = self.parse_substatement();
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
        let body = Box::new(self.parse_substatement()?);
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
        } else {
            None
        };

        if let Some(kind) = decl_kind {
            let first = self.parse_binding_pattern()?;
            if self.is_kw("in") || self.is_ident_word("of") {
                let of = self.is_ident_word("of");
                self.advance();
                let right = self.parse_assign()?;
                self.expect_punct(")")?;
                let body = Box::new(self.parse_loop_body()?);
                return Ok(Stmt::ForInOf { decl: Some(kind), left: first, right, of, is_await, body });
            }
            // Plain C-style for with a declaration init (possibly multiple declarators).
            let init_expr = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
            let mut decls = vec![(first, init_expr)];
            while self.eat_punct(",") {
                let pat = self.parse_binding_pattern()?;
                let init = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
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
        if self.is_kw("in") || self.is_ident_word("of") {
            let of = self.is_ident_word("of");
            self.advance();
            let right = self.parse_assign()?;
            self.expect_punct(")")?;
            let body = Box::new(self.parse_loop_body()?);
            let left = expr_to_pattern(&init_expr)
                .ok_or_else(|| ParseError { message: "invalid for-in/of target".into(), line: self.line() })?;
            return Ok(Stmt::ForInOf { decl: None, left, right, of, is_await, body });
        }
        self.expect_punct(";")?;
        self.finish_c_for(Some(Box::new(ForInit::Expr(init_expr))))
    }

    fn finish_c_for(&mut self, init: Option<Box<ForInit>>) -> Result<Stmt, ParseError> {
        let test = if self.is_punct(";") { None } else { Some(self.parse_expr()?) };
        self.expect_punct(";")?;
        let update = if self.is_punct(")") { None } else { Some(self.parse_expr()?) };
        self.expect_punct(")")?;
        let body = Box::new(self.parse_loop_body()?);
        Ok(Stmt::For { init, test, update, body })
    }

    /// A ModuleExportName: an identifier/keyword, or a string literal (`export { x as "y" }`).
    fn parse_module_export_name(&mut self) -> Result<String, ParseError> {
        match self.cur().clone() {
            Tok::Str(s) => {
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
        self.skip_import_attributes();
        Ok(spec)
    }
    /// Consume (and ignore) an import-attributes clause: `with { … }` or `assert { … }`.
    fn skip_import_attributes(&mut self) {
        if self.is_kw("with") || self.is_ident_word("assert") {
            self.advance();
            if self.eat_punct("{") {
                let mut depth = 1;
                while depth > 0 && !self.at_eof() {
                    if self.is_punct("{") {
                        depth += 1;
                    } else if self.is_punct("}") {
                        depth -= 1;
                    }
                    self.advance();
                }
            }
        }
    }

    fn parse_import(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // 'import'
        // Bare import: `import "spec";`
        if let Tok::Str(s) = self.cur().clone() {
            self.advance();
            let source = Rc::from(s.as_str());
            self.consume_semicolon()?;
            return Ok(Stmt::Import(ImportDecl { source, specs: Vec::new() }));
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
                let imported = self.parse_module_export_name()?;
                let local =
                    if self.is_ident_word("as") { self.advance(); self.parse_binding_ident_name()? } else { imported.clone() };
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
            let stmt = if self.is_kw("function") || (self.is_ident_word("async") && matches!(self.peek_kind(1), Tok::Keyword("function"))) {
                let is_async = self.eat_ident_word("async");
                Stmt::FuncDecl(Rc::new(self.parse_function(is_async)?))
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
            while !self.is_punct("}") {
                let local = self.parse_module_export_name()?;
                let exported =
                    if self.is_ident_word("as") { self.advance(); self.parse_module_export_name()? } else { local.clone() };
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
        Ok(Stmt::Try { block, handler, finalizer })
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
            while !self.is_punct("}") && !self.is_kw("case") && !self.is_kw("default") && !self.at_eof()
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
                    if expr_to_pattern(&left).is_none() {
                        return self.err("invalid destructuring assignment target");
                    }
                } else if !is_valid_assign_target(&left) {
                    return self.err("invalid assignment target");
                }
                // Assigning to `eval`/`arguments` is a SyntaxError in strict mode.
                if self.strict {
                    if let Expr::Ident(n) = &left {
                        if n == "eval" || n == "arguments" {
                            return self.err("cannot assign to 'eval' or 'arguments' in strict mode");
                        }
                    }
                }
                return Ok(Expr::Assign { op, target: Box::new(left), value: Box::new(value) });
            }
        }
        Ok(left)
    }

    fn parse_yield(&mut self) -> Result<Expr, ParseError> {
        self.advance(); // yield
        let delegate = self.eat_punct("*");
        // A bare `yield` has no argument (before a line terminator or a token that can't start one).
        let no_arg = (!delegate && self.nl_before())
            || matches!(
                self.cur(),
                Tok::Punct(";" | ")" | "]" | "}" | "," | ":") | Tok::Eof
            );
        let arg = if no_arg { None } else { Some(Box::new(self.parse_assign()?)) };
        Ok(Expr::Yield { delegate, arg })
    }

    fn parse_cond(&mut self) -> Result<Expr, ParseError> {
        let test = self.parse_binary(0)?;
        if self.eat_punct("?") {
            let cons = self.parse_assign()?;
            self.expect_punct(":")?;
            let alt = self.parse_assign()?;
            Ok(Expr::Cond { test: Box::new(test), cons: Box::new(cons), alt: Box::new(alt) })
        } else {
            Ok(test)
        }
    }

    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        while let Some((op, prec, right_assoc, logical)) = self.binary_op() {
            if prec < min_prec {
                break;
            }
            self.advance();
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_binary(next_min)?;
            left = if logical {
                Expr::Logical { op, left: Box::new(left), right: Box::new(right) }
            } else if op == "in" {
                // `#field in obj` is the ergonomic brand check, not a normal `in`.
                if let Expr::Ident(n) = &left {
                    if n.starts_with('#') {
                        left = Expr::PrivateIn { name: n.clone(), obj: Box::new(right) };
                        continue;
                    }
                }
                Expr::Binary { op, left: Box::new(left), right: Box::new(right) }
            } else {
                Expr::Binary { op, left: Box::new(left), right: Box::new(right) }
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
        // `await expr` (only a keyword inside an async function body).
        if self.in_async && self.is_ident_word("await") {
            self.advance();
            let arg = self.parse_unary()?;
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
            return Ok(Expr::Unary { op, arg: Box::new(arg) });
        }
        // Prefix ++/--
        if self.is_punct("++") || self.is_punct("--") {
            let op = if self.is_punct("++") { "++" } else { "--" };
            self.advance();
            let arg = self.parse_unary()?;
            self.check_strict_update_target(&arg)?;
            return Ok(Expr::Update { op, prefix: true, arg: Box::new(arg) });
        }
        self.parse_postfix()
    }

    fn check_strict_update_target(&self, arg: &Expr) -> Result<(), ParseError> {
        // The operand of `++`/`--` must be a simple assignment target (Identifier or member access).
        match arg {
            Expr::Ident(n) => {
                if self.strict && (n == "eval" || n == "arguments") {
                    return self.err("cannot increment/decrement 'eval' or 'arguments' in strict mode");
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
            return Ok(Expr::Update { op, prefix: false, arg: Box::new(expr) });
        }
        Ok(expr)
    }

    fn parse_lhs(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_member_expr()?;
        let mut had_optional = false;
        loop {
            if self.is_punct("(") {
                let args = self.parse_args()?;
                expr = Expr::Call { callee: Box::new(expr), args, optional: false };
            } else if self.eat_punct(".") {
                let name = self.parse_property_name_ident()?;
                expr = Expr::Member { obj: Box::new(expr), prop: name, optional: false };
            } else if self.eat_punct("[") {
                let index = self.parse_expr_allow_in()?;
                self.expect_punct("]")?;
                expr = Expr::Index { obj: Box::new(expr), index: Box::new(index), optional: false };
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
                    expr = Expr::Call { callee: Box::new(expr), args, optional: true };
                } else if self.eat_punct("[") {
                    let index = self.parse_expr_allow_in()?;
                    self.expect_punct("]")?;
                    expr =
                        Expr::Index { obj: Box::new(expr), index: Box::new(index), optional: true };
                } else {
                    let name = self.parse_property_name_ident()?;
                    expr = Expr::Member { obj: Box::new(expr), prop: name, optional: true };
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
                if self.fn_depth == 0 {
                    return self.err("new.target is only valid inside a function");
                }
                Expr::Undefined
            } else {
                let callee = self.parse_member_expr()?;
                let args = if self.is_punct("(") { self.parse_args()? } else { Vec::new() };
                Expr::New { callee: Box::new(callee), args }
            }
        } else {
            self.parse_primary()?
        };
        loop {
            if self.eat_punct(".") {
                let name = self.parse_property_name_ident()?;
                base = Expr::Member { obj: Box::new(base), prop: name, optional: false };
            } else if self.eat_punct("[") {
                let index = self.parse_expr_allow_in()?;
                self.expect_punct("]")?;
                base = Expr::Index { obj: Box::new(base), index: Box::new(index), optional: false };
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
                Ok(Expr::Regex { body: Rc::from(body.as_str()), flags: Rc::from(flags.as_str()) })
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
                let f = self.parse_function(false)?;
                Ok(Expr::Func(Rc::new(f)))
            }
            Tok::Keyword("class") => {
                let c = self.parse_class()?;
                Ok(Expr::Class(Rc::new(c)))
            }
            Tok::Keyword("super") => {
                self.advance();
                Ok(Expr::Super)
            }
            // `import(specifier)` (dynamic import) or `import.meta`.
            Tok::Keyword("import") => {
                self.advance();
                if self.eat_punct(".") {
                    self.expect_keyword_word("meta")?;
                    if !self.module {
                        return self.err("'import.meta' is only valid in a module");
                    }
                    Ok(Expr::ImportMeta)
                } else {
                    self.expect_punct("(")?;
                    let spec = self.parse_assign_allow_in()?;
                    // An optional second `import(spec, options)` argument is accepted and ignored.
                    if self.eat_punct(",") && !self.is_punct(")") {
                        let _ = self.parse_assign_allow_in()?;
                        self.eat_punct(",");
                    }
                    self.expect_punct(")")?;
                    Ok(Expr::ImportCall(Box::new(spec)))
                }
            }
            Tok::Ident(name) if name == "async" && matches!(self.peek_kind(1), Tok::Keyword("function")) =>
            {
                self.advance();
                let f = self.parse_function(true)?;
                Ok(Expr::Func(Rc::new(f)))
            }
            Tok::Ident(name) => {
                self.advance();
                match name.as_str() {
                    "undefined" => Ok(Expr::Undefined),
                    _ => Ok(Expr::Ident(name)),
                }
            }
            Tok::Punct("(") => {
                self.advance();
                let e = self.parse_expr_allow_in()?;
                self.expect_punct(")")?;
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
                    let tokens = tokenize(&src)
                        .map_err(|e| ParseError { message: e.message, line: e.line })?;
                    let mut sub = Parser { toks: tokens, pos: 0, strict: self.strict, depth: self.depth, in_generator: self.in_generator, in_async: self.in_async, no_in: false, module: self.module, fn_depth: self.fn_depth, iter_depth: self.iter_depth, switch_depth: self.switch_depth, labels: Vec::new(), decl_scopes: vec![DeclScope::default()] };
                    sub.parse_expr()?
                }
            };
            expr = Some(match expr {
                None => piece,
                Some(left) => {
                    Expr::Binary { op: "+", left: Box::new(left), right: Box::new(piece) }
                }
            });
        }
        Ok(expr.unwrap_or(Expr::Str(Rc::from(""))))
    }

    fn build_tagged_template(&mut self, tag: Expr, parts: Vec<TplPart>) -> Result<Expr, ParseError> {
        let mut quasis = Vec::new();
        let mut subs = Vec::new();
        for part in parts {
            match part {
                TplPart::Str { cooked, raw } => quasis.push((Some(cooked), raw)),
                TplPart::Sub(src) => {
                    let tokens = tokenize(&src)
                        .map_err(|e| ParseError { message: e.message, line: e.line })?;
                    let mut sub = Parser { toks: tokens, pos: 0, strict: self.strict, depth: self.depth, in_generator: self.in_generator, in_async: self.in_async, no_in: false, module: self.module, fn_depth: self.fn_depth, iter_depth: self.iter_depth, switch_depth: self.switch_depth, labels: Vec::new(), decl_scopes: vec![DeclScope::default()] };
                    subs.push(sub.parse_expr()?);
                }
            }
        }
        Ok(Expr::TaggedTemplate { tag: Box::new(tag), quasis, subs })
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
                && !matches!(self.peek_kind(1), Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("}") | Tok::Punct("("))
            {
                let is_get = self.is_ident_word("get");
                self.advance();
                let key = self.parse_prop_key()?;
                let func = self.parse_accessor_function(is_get)?;
                props.push(if is_get {
                    PropDef::Getter { key, func: Rc::new(func) }
                } else {
                    PropDef::Setter { key, func: Rc::new(func) }
                });
                if !self.eat_punct(",") {
                    break;
                }
                continue;
            }
            // `async` / generator `*` method prefixes (async only when followed by a key start).
            let is_async = self.is_ident_word("async")
                && !matches!(
                    self.peek_kind(1),
                    Tok::Punct(":") | Tok::Punct(",") | Tok::Punct("}") | Tok::Punct("(") | Tok::Punct("=")
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
                props.push(PropDef::KeyValue { key, value: Expr::Func(Rc::new(func)) });
            } else if self.eat_punct(":") {
                // Two `__proto__: value` data properties in one literal are a SyntaxError.
                let is_proto = matches!(&key, PropKey::Ident(n) if n == "__proto__")
                    || matches!(&key, PropKey::Str(s) if &**s == "__proto__");
                if is_proto {
                    if proto_seen {
                        return self.err("duplicate __proto__ property in object literal");
                    }
                    proto_seen = true;
                }
                let value = self.parse_assign()?;
                props.push(PropDef::KeyValue { key, value });
            } else {
                // Shorthand `{ x }`, or CoverInitializedName `{ x = default }` (only meaningful in
                // a destructuring assignment target). Only valid when key is a plain identifier.
                match &key {
                    PropKey::Ident(name) => {
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
                let e = self.parse_assign()?;
                self.expect_punct("]")?;
                Ok(PropKey::Computed(e))
            }
            _ => self.err("expected property key"),
        }
    }

    // ----- functions --------------------------------------------------------------------------

    fn parse_function(&mut self, is_async: bool) -> Result<Function, ParseError> {
        self.eat_kw("function");
        let is_generator = self.eat_punct("*");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            self.advance();
            Some(n)
        } else {
            None
        };
        let params = self.parse_params()?;
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        let (body, is_strict) = self.parse_function_body()?;
        self.in_generator = sg;
        self.in_async = sa;
        let strict = is_strict || self.strict;
        // Duplicate parameters are an error in strict mode, or whenever the list is non-simple
        // (defaults / rest / destructuring).
        if strict || params_complex(&params) {
            if let Some(dup) = duplicate_name(&param_names(&params)) {
                return self.err(format!("duplicate parameter name '{dup}'"));
            }
        }
        Ok(Function {
            name,
            params,
            body,
            is_arrow: false,
            is_strict: strict,
            expr_body: false,
            is_generator,
            is_async,
        })
    }

    // ----- classes ----------------------------------------------------------------------------

    fn parse_class(&mut self) -> Result<Class, ParseError> {
        self.eat_kw("class");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            self.advance();
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
            if let Some(m) = self.parse_class_member()? {
                members.push(m);
            }
        }
        self.strict = saved;
        self.expect_punct("}")?;
        validate_class(&members).map_err(|m| ParseError { message: m, line: self.line() })?;
        Ok(Class { name, superclass, members })
    }

    /// True when the token `ahead` of the cursor ends a member head — i.e. the current contextual
    /// word (`static`/`get`/`set`/`async`) is actually the member *name*, not a modifier.
    fn next_is_member_terminator(&self, ahead: usize) -> bool {
        matches!(
            self.peek_kind(ahead),
            Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}")
        )
    }

    fn parse_class_member(&mut self) -> Result<Option<ClassMember>, ParseError> {
        if self.eat_punct(";") {
            return Ok(None);
        }
        let mut is_static = false;
        if self.is_ident_word("static") && !self.next_is_member_terminator(1) {
            self.advance();
            is_static = true;
        }
        // `static { ... }` initialization block.
        if is_static && self.is_punct("{") {
            self.advance();
            let body = self.parse_block_body()?;
            let func = Function {
                name: None,
                params: Vec::new(),
                body,
                is_arrow: false,
                is_strict: true,
                expr_body: false,
                is_generator: false,
                is_async: false,
            };
            return Ok(Some(ClassMember {
                key: PropKey::Ident(String::new()),
                kind: MemberKind::StaticBlock,
                is_static: true,
                func: Some(Rc::new(func)),
                value: None,
            }));
        }
        let mut kind = MemberKind::Method;
        if (self.is_ident_word("get") || self.is_ident_word("set")) && !self.next_is_member_terminator(1)
        {
            kind = if self.is_ident_word("get") { MemberKind::Get } else { MemberKind::Set };
            self.advance();
        }
        let is_async = self.is_ident_word("async") && !self.next_is_member_terminator(1);
        if is_async {
            self.advance();
        }
        let is_generator = self.eat_punct("*");

        let key = self.parse_prop_key()?;

        if self.is_punct("(") {
            let func = self.parse_method_function_kind(is_generator, is_async)?;
            if matches!(kind, MemberKind::Get | MemberKind::Set) {
                check_accessor_arity(&func, kind == MemberKind::Get)
                    .map_err(|m| ParseError { message: m, line: self.line() })?;
            }
            let kind = if kind == MemberKind::Method && !is_static && key_is(&key, "constructor") {
                MemberKind::Constructor
            } else {
                kind
            };
            Ok(Some(ClassMember { key, kind, is_static, func: Some(Rc::new(func)), value: None }))
        } else {
            // Field declaration.
            let value = if self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
            self.consume_semicolon()?;
            Ok(Some(ClassMember { key, kind: MemberKind::Field, is_static, func: None, value }))
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
        let params = self.parse_params()?;
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        let (body, is_strict) = self.parse_function_body()?;
        self.in_generator = sg;
        self.in_async = sa;
        Ok(Function {
            name: None,
            params,
            body,
            is_arrow: false,
            is_strict: is_strict || self.strict,
            expr_body: false,
            is_generator,
            is_async,
        })
    }

    fn parse_accessor_function(&mut self, is_get: bool) -> Result<Function, ParseError> {
        let f = self.parse_method_function()?;
        check_accessor_arity(&f, is_get).map_err(|m| ParseError { message: m, line: self.line() })?;
        Ok(f)
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        self.expect_punct("(")?;
        let mut params = Vec::new();
        while !self.is_punct(")") {
            let rest = self.eat_punct("...");
            let pattern = self.parse_binding_pattern()?;
            let default =
                if !rest && self.eat_punct("=") { Some(self.parse_assign()?) } else { None };
            params.push(Param { pattern, default, rest });
            if rest || !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(params)
    }

    fn parse_function_body(&mut self) -> Result<(Vec<Stmt>, bool), ParseError> {
        self.expect_punct("{")?;
        let saved_strict = self.strict;
        let inner_strict = matches!(self.cur(), Tok::Str(s) if s == "use strict");
        if inner_strict {
            self.strict = true;
        }
        // A function body is a fresh context for return/break/continue and labels.
        let (siter, sswitch) = (self.iter_depth, self.switch_depth);
        let slabels = std::mem::take(&mut self.labels);
        self.fn_depth += 1;
        self.iter_depth = 0;
        self.switch_depth = 0;
        let body = self.parse_block_body();
        self.fn_depth -= 1;
        self.iter_depth = siter;
        self.switch_depth = sswitch;
        self.labels = slabels;
        let body = body?;
        let result_strict = self.strict;
        self.strict = saved_strict;
        Ok((body, result_strict))
    }

    // ----- arrow functions --------------------------------------------------------------------

    fn try_parse_arrow(&mut self) -> Result<Option<Expr>, ParseError> {
        // Optional `async` prefix (on the same line) for an async arrow.
        let async_arrow = self.is_ident_word("async")
            && !self.toks.get(self.pos + 1).map(|t| t.nl_before).unwrap_or(true)
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
                let params = vec![Param { pattern: Pattern::Ident(name), default: None, rest: false }];
                return Ok(Some(self.finish_arrow(params, async_arrow)?));
            }
        }
        // `( params ) => ...`
        if matches!(self.peek_kind(base), Tok::Punct("(")) {
            if let Some(close) = self.matching_paren(self.pos + base) {
                if matches!(self.toks.get(close + 1).map(|t| &t.kind), Some(Tok::Punct("=>")))
                    && !self.toks[close + 1].nl_before
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
            let (body, is_strict) = self.parse_function_body()?;
            Function {
                name: None,
                params,
                body,
                is_arrow: true,
                is_strict: is_strict || self.strict,
                expr_body: false,
                is_generator: false,
                is_async,
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
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | ">>>=" | "&=" | "|="
            | "^=" | "&&=" | "||=" | "??="
    )
}

fn is_valid_assign_target(e: &Expr) -> bool {
    matches!(e, Expr::Ident(_) | Expr::Member { .. } | Expr::Index { .. })
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
    // (name, is_static) → the member kinds declared under that private name.
    let mut private: Vec<((String, bool), Vec<MemberKind>)> = Vec::new();
    for m in members {
        if matches!(m.kind, MemberKind::Constructor) {
            ctor_count += 1;
            if ctor_count > 1 {
                return Err("a class may only have one constructor".into());
            }
        }
        if let PropKey::Ident(name) = &m.key {
            if name.starts_with('#') {
                if name == "#constructor" {
                    return Err("'#constructor' is a reserved private name".into());
                }
                let entry = private.iter_mut().find(|((n, s), _)| n == name && *s == m.is_static);
                match entry {
                    Some((_, kinds)) => kinds.push(m.kind),
                    None => private.push(((name.clone(), m.is_static), vec![m.kind])),
                }
            }
        }
    }
    for ((name, _), kinds) in &private {
        let ok = kinds.len() == 1
            || (kinds.len() == 2
                && kinds.iter().any(|k| matches!(k, MemberKind::Get))
                && kinds.iter().any(|k| matches!(k, MemberKind::Set)));
        if !ok {
            return Err(format!("duplicate private name '{name}'"));
        }
    }
    Ok(())
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
    params.iter().any(|p| p.default.is_some() || p.rest || !matches!(p.pattern, Pattern::Ident(_)))
}
fn duplicate_name(names: &[String]) -> Option<String> {
    for (idx, n) in names.iter().enumerate() {
        if names[..idx].contains(n) {
            return Some(n.clone());
        }
    }
    None
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
            for el in elems {
                match el {
                    ArrayElem::Hole => out.push(ArrayPatElem::Hole),
                    ArrayElem::Spread(t) => out.push(ArrayPatElem::Rest(expr_to_pattern(t)?)),
                    ArrayElem::Item(Expr::Assign { op: "=", target, value }) => {
                        out.push(ArrayPatElem::Elem {
                            pattern: expr_to_pattern(target)?,
                            default: Some((**value).clone()),
                        })
                    }
                    ArrayElem::Item(t) => {
                        out.push(ArrayPatElem::Elem { pattern: expr_to_pattern(t)?, default: None })
                    }
                }
            }
            Some(Pattern::Array(out))
        }
        Expr::Object(props) => {
            let mut pat = ObjectPat { props: Vec::new(), rest: None };
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } => {
                        let (value, default) = match value {
                            Expr::Assign { op: "=", target, value: d } => {
                                (expr_to_pattern(target)?, Some((**d).clone()))
                            }
                            v => (expr_to_pattern(v)?, None),
                        };
                        pat.props.push(ObjPatProp { key: key.clone(), value, default });
                    }
                    PropDef::Spread(Expr::Ident(name)) => pat.rest = Some(name.clone()),
                    _ => return None,
                }
            }
            Some(Pattern::Object(pat))
        }
        // A member expression (`o.p` / `o[k]`) is a valid assignment target.
        Expr::Member { optional: false, .. } | Expr::Index { optional: false, .. } => {
            Some(Pattern::Member(Box::new(e.clone())))
        }
        _ => None,
    }
}
