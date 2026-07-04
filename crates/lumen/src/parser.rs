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
    parse_script_eval(src, strict, false, false, &[])
}

/// Parse eval code. Like [`parse_script`], but `allow_new_target` permits a top-level `new.target`
/// (a direct eval whose caller is inside a function).
pub fn parse_script_eval(
    src: &str,
    strict: bool,
    allow_new_target: bool,
    allow_super: bool,
    private_names: &[String],
) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(src).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        src_chars: Rc::new(src.chars().collect()),
        strict,
        depth: 0,
        in_generator: false,
        in_async: false,
        in_params: false,
        no_in: false,
        module: false,
        fn_depth: 0,
        nonarrow_fn_depth: 0,
        iter_depth: 0,
        switch_depth: 0,
        labels: Vec::new(),
        iter_labels: Vec::new(),
        decl_scopes: vec![DeclScope {
            fn_boundary: true,
            ..Default::default()
        }],
        next_scope_is_fn_boundary: false,
        allow_new_target,
        top_level: false,
        super_prop_ok: allow_super,
        super_call_ok: allow_super,
        in_derived_class: false,
        in_case_clause: false,
        no_arguments_refs: false,
        proto_dups: Vec::new(),
        last_paren: false,
        single_stmt: false,
        in_static_block: false,
    };
    let strict_prologue = p.has_use_strict_prologue();
    p.strict = p.strict || strict_prologue;
    let body = p.parse_stmts_until_eof()?;
    // A direct eval sees the private names visible where it was called.
    let mut st: Vec<Vec<String>> = Vec::new();
    if !private_names.is_empty() {
        st.push(private_names.to_vec());
    }
    pn_stmts(&body, &mut st).map_err(|message| ParseError { message, line: 0 })?;
    // A super call is never valid in script/global code (only a derived constructor, or a direct
    // eval inside one, may contain it).
    if !allow_super {
        if let Some(message) = crate::eval::top_level_super_call_error(&body) {
            return Err(ParseError {
                message: message.into(),
                line: 0,
            });
        }
    }
    Ok(body)
}

/// Parse a module (always strict; `import`/`export` are allowed only here). Modules permit top-level
/// `await`, so `await` is treated as a keyword at the module's top level.
pub fn parse_module(src: &str) -> Result<Vec<Stmt>, ParseError> {
    let tokens = crate::lexer::tokenize_goal(src, false).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        src_chars: Rc::new(src.chars().collect()),
        strict: true,
        depth: 0,
        in_generator: false,
        in_async: true,
        in_params: false,
        no_in: false,
        module: true,
        fn_depth: 0,
        nonarrow_fn_depth: 0,
        iter_depth: 0,
        switch_depth: 0,
        labels: Vec::new(),
        iter_labels: Vec::new(),
        decl_scopes: vec![DeclScope {
            fn_boundary: true,
            ..Default::default()
        }],
        next_scope_is_fn_boundary: false,
        allow_new_target: false,
        top_level: false,
        super_prop_ok: false,
        super_call_ok: false,
        in_derived_class: false,
        in_case_clause: false,
        no_arguments_refs: false,
        proto_dups: Vec::new(),
        last_paren: false,
        single_stmt: false,
        in_static_block: false,
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
                        ImportSpec::Default(n)
                        | ImportSpec::Namespace(n)
                        | ImportSpec::DeferNamespace(n)
                        | ImportSpec::Source(n) => n,
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
    // A body-top-level function declaration is var-scoped inside a *function body* (unlike module
    // top level): it may legally share a parameter's name.
    lexical.retain(|l| {
        !body
            .iter()
            .any(|s| matches!(s, Stmt::FuncDecl(f) if f.name.as_deref() == Some(l.as_str())))
    });
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
    /// The source as chars, for slicing function source text by token offsets.
    src_chars: Rc<Vec<char>>,
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
    /// Like `fn_depth`, but arrows are transparent — `new.target` needs an enclosing non-arrow.
    nonarrow_fn_depth: u32,
    iter_depth: u32,
    switch_depth: u32,
    /// Active labels in scope (reset at function boundaries).
    labels: Vec<String>,
    /// The subset of `labels` whose labelled statement is an iteration statement (valid
    /// `continue` targets).
    iter_labels: Vec<String>,
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
    /// True where a `SuperCall` (`super(...)`) is syntactically allowed: the constructor body of a
    /// class with a heritage clause (an arrow inherits it; everything else resets it).
    super_call_ok: bool,
    /// True while parsing the members of a class that has an `extends` clause.
    in_derived_class: bool,
    /// True while parsing the statements directly inside a switch case/default clause.
    in_case_clause: bool,
    /// True while `arguments` may not be referenced (a class static block; unlike the `await`
    /// reservation this survives arrow bodies — `Contains` descends arrows).
    no_arguments_refs: bool,
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
    /// Inside a `static { … }` class block, where `await` is reserved entirely (neither an
    /// identifier nor an await expression). Cleared by nested function boundaries.
    in_static_block: bool,
}

#[derive(Default)]
struct DeclScope {
    lexical: Vec<String>,
    var: Vec<String>,
    /// Sloppy-mode plain function declarations in this block (Annex B): duplicates among
    /// themselves are fine, but they conflict with same-block lexicals and vars.
    fn_lexical: Vec<String>,
    /// A `var` hoists up to (and conflicts with lexicals only up to) the nearest such boundary — the
    /// program/function top. Block / `for` / `switch` scopes are not boundaries.
    fn_boundary: bool,
}

impl Parser {
    fn cur_start(&self) -> u32 {
        self.toks.get(self.pos).map(|t| t.start).unwrap_or(0)
    }
    fn prev_end(&self) -> u32 {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].end
        }
    }
    /// The source text between two char offsets (a function's `toString` view).
    fn src_slice(&self, start: u32, end: u32) -> Option<Rc<str>> {
        let (s, e) = (start as usize, end as usize);
        if s <= e && e <= self.src_chars.len() {
            Some(Rc::from(self.src_chars[s..e].iter().collect::<String>()))
        } else {
            None
        }
    }

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
        // An escaped spelling (`d\u006f`) is never usable AS the keyword (only as e.g. a
        // property name, which matches `Tok::Keyword` directly).
        matches!(self.cur(), Tok::Keyword(x) if *x == k) && !self.cur_escaped()
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
        } else if in_block {
            // Annex B block function: duplicates with other block functions are allowed, but the
            // name must not collide with a lexical or var declared in the same block.
            let clash = {
                let s = self.decl_scopes.last().unwrap();
                s.lexical.iter().any(|n| n == name) || s.var.iter().any(|n| n == name)
            };
            if clash {
                return self.err(format!("Identifier '{name}' has already been declared"));
            }
            self.decl_scopes
                .last_mut()
                .unwrap()
                .fn_lexical
                .push(name.to_string());
            Ok(())
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
        // eval/arguments are legal shorthand *references* even in strict mode — they are only
        // banned when the literal is reinterpreted as an assignment pattern (checked there).
        if self.strict && is_strict_reserved_binding(name) && name != "eval" && name != "arguments"
        {
            return self.err(format!(
                "'{name}' cannot be used as a shorthand property in strict mode"
            ));
        }
        if ((self.in_async || self.in_static_block || self.module) && name == "await")
            || (self.in_generator && name == "yield")
        {
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
        if let Some(s) = self.decl_scopes.pop() {
            // VarDeclaredNames accumulate through block scopes (vars hoist), so declarations in
            // an enclosing block still conflict with them.
            if !s.fn_boundary {
                if let Some(parent) = self.decl_scopes.last_mut() {
                    parent.var.extend(s.var);
                }
            }
        }
    }
    /// Record a lexical (`let`/`const`/`class`) binding; error on redeclaration in this scope.
    fn declare_lexical(&mut self, name: &str) -> Result<(), ParseError> {
        let conflict = {
            let s = self.decl_scopes.last().unwrap();
            s.lexical.iter().any(|n| n == name)
                || s.var.iter().any(|n| n == name)
                || s.fn_lexical.iter().any(|n| n == name)
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
            if scope.lexical.iter().any(|n| n == name) || scope.fn_lexical.iter().any(|n| n == name)
            {
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
        self.prologue_has_use_strict(0)
    }

    /// Whether the directive prologue starting at token `from` contains a "use strict" directive.
    /// The prologue is the leading run of string-literal expression statements (terminated by `;`
    /// or ASI) — a later directive still makes the earlier prologue strings strict.
    fn prologue_has_use_strict(&self, from: usize) -> bool {
        let mut i = from;
        loop {
            let Some(tok) = self.toks.get(i) else {
                return false;
            };
            let Tok::Str(str_val) = &tok.kind else {
                return false;
            };
            let next = self.toks.get(i + 1);
            let semi = matches!(next.map(|t| &t.kind), Some(Tok::Punct(";")));
            let asi =
                next.is_none_or(|t| t.nl_before || matches!(t.kind, Tok::Punct("}") | Tok::Eof));
            if !semi && !asi {
                return false;
            }
            // The directive must be the exact code units `use strict` — a string that used
            // escapes or line continuations does not count.
            if str_val == "use strict" && !tok.escaped {
                return true;
            }
            i += if semi { 2 } else { 1 };
        }
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
        if matches!(self.cur(), Tok::Keyword(_)) && self.cur_escaped() {
            return self.err("keywords must not contain escape sequences");
        }
        if self.in_static_block && self.is_kw("return") {
            return self.err("'return' is not allowed directly in a class static block");
        }
        // `import`/`export` declarations are valid only as top-level ModuleItems. Capture the flag
        // and clear it: any statement we recurse into (block, if/loop body, label, switch case) is a
        // nested context where an import/export declaration is a SyntaxError.
        let at_top = self.top_level;
        self.top_level = false;
        let in_case = std::mem::take(&mut self.in_case_clause);
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
                    && !self.cur_escaped()
                    && self.starts_let_decl()
                    && (!single_stmt || matches!(self.peek_kind(1), Tok::Punct("["))) =>
            {
                self.parse_var_decl(DeclKind::Let)
            }
            Tok::Ident(w) if w == "using" && self.starts_using_decl() => {
                self.check_using_context(at_top, in_case)?;
                self.parse_var_decl(DeclKind::Using)
            }
            Tok::Ident(w) if w == "await" && self.in_async && self.starts_await_using() => {
                self.check_using_context(at_top, in_case)?;
                self.parse_var_decl(DeclKind::AwaitUsing)
            }
            Tok::Keyword("function") => {
                let f = self.parse_function(false, false, self.cur_start())?;
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
                    && matches!(self.peek_kind(1), Tok::Keyword("function"))
                    && !self
                        .toks
                        .get(self.pos + 1)
                        .map(|t| t.nl_before)
                        .unwrap_or(true) =>
            {
                let start = self.cur_start();
                self.advance();
                let f = self.parse_function(true, false, start)?;
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
                    // `continue label` must target a label on an iteration statement.
                    Some(l) if !self.iter_labels.contains(l) => {
                        return self.err("continue label does not target an iteration statement")
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
                // Collect the whole label chain, so an Annex B labelled FunctionDeclaration is
                // recognized through any number of labels.
                let _ = &name;
                let mut chain: Vec<String> = Vec::new();
                loop {
                    let n = match self.cur() {
                        Tok::Ident(n) if matches!(self.peek_kind(1), Tok::Punct(":")) => n.clone(),
                        _ => break,
                    };
                    if let Err(e) = self.check_label_name(&n) {
                        for _ in 0..chain.len() {
                            self.labels.pop();
                        }
                        return Err(e);
                    }
                    if self.labels.contains(&n) {
                        for _ in 0..chain.len() {
                            self.labels.pop();
                        }
                        return self.err(format!("label '{n}' has already been declared"));
                    }
                    self.advance();
                    self.advance();
                    self.labels.push(n.clone());
                    chain.push(n);
                }
                // `continue label` may only target a label whose statement is (a label chain
                // over) an iteration statement.
                let iter_ahead = matches!(
                    self.cur(),
                    Tok::Keyword("for") | Tok::Keyword("while") | Tok::Keyword("do")
                );
                if iter_ahead {
                    for l in &chain {
                        self.iter_labels.push(l.clone());
                    }
                }
                // Annex B: LabelledItem may be a plain FunctionDeclaration in sloppy mode.
                let body = if !self.strict && self.is_kw("function") {
                    match self.parse_function(false, false, self.cur_start()) {
                        Ok(f) if f.is_generator || f.is_async => {
                            self.err("a labelled function declaration must be a plain function")
                        }
                        Ok(f) => {
                            let declared = match &f.name {
                                Some(n) => self.declare_fn_decl(n, false, false),
                                None => Ok(()),
                            };
                            declared.map(|_| Stmt::FuncDecl(Rc::new(f)))
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    self.parse_substatement(true)
                };
                for _ in 0..chain.len() {
                    self.labels.pop();
                    if iter_ahead {
                        self.iter_labels.pop();
                    }
                }
                let mut stmt = body?;
                for label in chain.into_iter().rev() {
                    stmt = Stmt::Labeled {
                        label,
                        body: Box::new(stmt),
                    };
                }
                Ok(stmt)
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

    /// A `using`/`await using` declaration is illegal at the top level of a script (or eval) and
    /// directly inside a switch case/default statement list.
    fn check_using_context(&mut self, at_top: bool, in_case: bool) -> Result<(), ParseError> {
        if at_top && !self.module && self.fn_depth == 0 {
            return self.err("'using' declaration is not allowed at the top level of a script");
        }
        if in_case {
            return self.err("'using' declaration is not allowed directly in a case clause");
        }
        Ok(())
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
        // `let` may not be bound by a lexical declaration (even in sloppy mode).
        if kind != DeclKind::Var && names.iter().any(|n| n == "let") {
            return self.err("'let' is disallowed as a lexically bound name");
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
                // `await`/`yield` are reserved as bindings inside async/generator bodies (and
                // `await` inside a class static block).
                if ((self.in_async || self.in_static_block) && name == "await")
                    || (self.in_generator && name == "yield")
                {
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
            self.reject_private_key(&key)?;
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
        } else if self.is_ident_word("let") && !self.cur_escaped() && self.starts_let_decl() {
            self.advance();
            Some(DeclKind::Let)
        } else if self.is_ident_word("using")
            && self.starts_using_decl()
            && (!matches!(self.peek_kind(1), Tok::Ident(w) if w == "of")
                || matches!(self.peek_kind(2), Tok::Punct("=")))
        {
            // `for (using of = ...)` declares a resource named `of`; any other `for (using of`
            // keeps `using` as a plain for-of left side.
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
                    if names.iter().any(|n| n == "let") {
                        return self.err("'let' is disallowed as a lexically bound name");
                    }
                    for n in &names {
                        self.declare_lexical(n)?;
                    }
                }
                let right = if of {
                    self.parse_assign()?
                } else {
                    // A for-in head's right side is a full Expression (comma allowed).
                    self.parse_expr_allow_in()?
                };
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
            // Plain C-style for with a declaration init (possibly multiple declarators). The
            // initializer is parsed [NoIn] so a for-in head's `in` stays visible.
            let init_expr = if self.eat_punct("=") {
                let saved = std::mem::replace(&mut self.no_in, true);
                let e = self.parse_assign();
                self.no_in = saved;
                Some(e?)
            } else {
                None
            };
            let mut decls = vec![(first, init_expr)];
            // Annex B: `for (var a = 0 in expr)` — a lone var declarator may carry an initializer
            // in a sloppy for-in head; it runs before the loop.
            if !self.strict
                && kind == DeclKind::Var
                && decls.len() == 1
                && decls[0].1.is_some()
                && matches!(decls[0].0, Pattern::Ident(_))
                && self.is_kw("in")
            {
                self.advance();
                let right = self.parse_expr_allow_in()?;
                self.expect_punct(")")?;
                let body = Box::new(self.parse_loop_body()?);
                let (pat, init) = decls.pop().unwrap();
                return Ok(Stmt::Block(vec![
                    Stmt::VarDecl {
                        kind: DeclKind::Var,
                        decls: vec![(pat.clone(), init)],
                    },
                    Stmt::ForInOf {
                        decl: Some(DeclKind::Var),
                        left: pat,
                        right,
                        of: false,
                        is_await,
                        body,
                    },
                ]));
            }
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
            // The head's bound names live in the for statement's own scope: a lexical head name
            // redeclared by a `var` in the body is a SyntaxError.
            {
                let mut names = Vec::new();
                for (pat, _) in &decls {
                    pattern_names(pat, &mut names);
                }
                if kind != DeclKind::Var && names.iter().any(|n| n == "let") {
                    return self.err("'let' is disallowed as a lexically bound name");
                }
                for n in &names {
                    match kind {
                        DeclKind::Var => self.declare_var(n, true)?,
                        _ => self.declare_lexical(n)?,
                    }
                }
            }
            return self.finish_c_for(Some(Box::new(ForInit::VarDecl { kind, decls })));
        }

        // No declaration: either empty init or an expression init.
        if self.eat_punct(";") {
            return self.finish_c_for(None);
        }
        let proto_mark = self.proto_dups.len();
        // A head starting with the unescaped, unparenthesized token `async` can't be a plain
        // for-of LHS (ambiguity with an async arrow head); note it before parsing.
        let bare_async_head =
            matches!(self.cur(), Tok::Ident(w) if *w == "async") && !self.cur_escaped();
        let init_expr = self.parse_expr_no_in()?;
        if self.is_kw("in") || (self.is_ident_word("of") && !self.cur_escaped()) {
            let of = self.is_ident_word("of") && !self.cur_escaped();
            self.advance();
            let right = if of {
                self.parse_assign()?
            } else {
                self.parse_expr_allow_in()?
            };
            self.expect_punct(")")?;
            let body = Box::new(self.parse_loop_body()?);
            // `for (async of ...)` is ambiguous with an async arrow head: a bare, unescaped
            // `async` LHS in a plain for-of is a SyntaxError (parenthesize or escape it;
            // for-await has no ambiguity).
            if of
                && !is_await
                && bare_async_head
                && matches!(&init_expr, Expr::Ident(n) if n == "async")
            {
                return Err(ParseError {
                    message: "'async' as a for-of left side must be parenthesized".into(),
                    line: self.line(),
                });
            }
            let left = match &init_expr {
                // A destructuring assignment head keeps its expression form: the full
                // AssignmentPattern semantics (iterator close, member and member-rest targets)
                // live in the runtime's assign_to_target.
                e @ (Expr::Array(_) | Expr::Object(_)) => {
                    if !valid_assignment_pattern(e) {
                        return Err(ParseError {
                            message: "invalid for-in/of target".into(),
                            line: self.line(),
                        });
                    }
                    // Reinterpreted as a pattern: forgive deferred literal-only errors
                    // (duplicate __proto__, CoverInitializedName).
                    self.proto_dups.truncate(proto_mark);
                    Pattern::Member(Box::new(init_expr.clone()))
                }
                _ => match expr_to_pattern(&init_expr) {
                    Some(p) => p,
                    // Annex B: a sloppy CallExpression head parses; assignment throws at runtime.
                    None if !self.strict && matches!(init_expr, Expr::Call { .. }) => {
                        Pattern::Member(Box::new(init_expr.clone()))
                    }
                    None => {
                        return Err(ParseError {
                            message: "invalid for-in/of target".into(),
                            line: self.line(),
                        })
                    }
                },
            };
            if self.strict && pattern_strict_banned(&init_expr) {
                return self.err("cannot assign to 'eval' or 'arguments' in strict mode");
            }
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
            // A contextual keyword in a syntactic position cannot use `\u` escapes.
            if self.cur_escaped() {
                return self.err(format!("'{w}' must not contain escape sequences"));
            }
            self.advance();
            Ok(())
        } else {
            self.err(format!("expected '{w}'"))
        }
    }
    fn parse_module_specifier(&mut self) -> Result<(Rc<str>, Option<String>), ParseError> {
        let spec = match self.cur().clone() {
            Tok::Str(s) => {
                self.advance();
                Rc::from(s.as_str())
            }
            _ => return self.err("expected a module specifier string"),
        };
        let attr_type = self.parse_import_attributes()?;
        Ok((spec, attr_type))
    }
    /// Parse (and validate) an optional import-attributes clause: `with { key: "value", … }` (or the
    /// legacy `assert { … }`). The attribute values must be string literals and the keys must be
    /// unique — a duplicate key is an early SyntaxError.
    fn parse_import_attributes(&mut self) -> Result<Option<String>, ParseError> {
        if !self.is_kw("with") && !self.is_ident_word("assert") {
            return Ok(None);
        }
        self.advance(); // 'with' / 'assert'
        self.expect_punct("{")?;
        let mut keys: Vec<String> = Vec::new();
        let mut attr_type = None;
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
            keys.push(key.clone());
            self.expect_punct(":")?;
            match self.cur().clone() {
                Tok::Str(v) => {
                    if key == "type" {
                        attr_type = Some(v.clone());
                    }
                    self.advance();
                }
                _ => return self.err("import attribute value must be a string literal"),
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("}")?;
        Ok(attr_type)
    }

    /// Parse the `( specifier [, options] )` of a dynamic-import call (any phase). The optional
    /// second options argument is accepted and ignored.
    fn parse_import_call_args(&mut self) -> Result<(Expr, Option<Expr>), ParseError> {
        self.expect_punct("(")?;
        let spec = self.parse_assign_allow_in()?;
        let mut options = None;
        if self.eat_punct(",") && !self.is_punct(")") {
            options = Some(self.parse_assign_allow_in()?);
            self.eat_punct(",");
        }
        self.expect_punct(")")?;
        Ok((spec, options))
    }

    fn parse_import(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // 'import'
                        // Bare import: `import "spec";`
        if let Tok::Str(s) = self.cur().clone() {
            self.advance();
            let source = Rc::from(s.as_str());
            let attr_type = self.parse_import_attributes()?;
            self.consume_semicolon()?;
            return Ok(Stmt::Import(ImportDecl {
                source,
                specs: Vec::new(),
                attr_type,
            }));
        }
        let mut specs = Vec::new();
        let mut need_from = true;
        // `import source x from "spec"` — a source-phase import. `import source from "spec"` is
        // instead a *default* import named `source` (lookahead: `from` + a specifier string).
        if self.is_ident_word("source")
            && !self.cur_escaped()
            && matches!(self.peek_kind(1), Tok::Ident(_))
            && !(matches!(self.peek_kind(1), Tok::Ident(w) if w == "from")
                && matches!(self.peek_kind(2), Tok::Str(_)))
        {
            self.advance(); // source
            let local = self.parse_binding_ident_name()?;
            self.expect_keyword_word("from")?;
            let (source, attr_type) = self.parse_module_specifier()?;
            self.consume_semicolon()?;
            return Ok(Stmt::Import(ImportDecl {
                source,
                specs: vec![ImportSpec::Source(local)],
                attr_type,
            }));
        }
        // `import defer * as ns from "spec"` — a deferred namespace import.
        if self.is_ident_word("defer")
            && !self.cur_escaped()
            && matches!(self.peek_kind(1), Tok::Punct("*"))
        {
            self.advance(); // defer
            self.expect_punct("*")?;
            self.expect_keyword_word("as")?;
            specs.push(ImportSpec::DeferNamespace(self.parse_binding_ident_name()?));
            self.expect_keyword_word("from")?;
            let (source, attr_type) = self.parse_module_specifier()?;
            self.consume_semicolon()?;
            return Ok(Stmt::Import(ImportDecl {
                source,
                specs,
                attr_type,
            }));
        }
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
                    if self.cur_escaped() {
                        return self.err("'as' must not contain escape sequences");
                    }
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
        let (source, attr_type) = self.parse_module_specifier()?;
        self.consume_semicolon()?;
        Ok(Stmt::Import(ImportDecl {
            source,
            specs,
            attr_type,
        }))
    }

    fn parse_export(&mut self) -> Result<Stmt, ParseError> {
        self.advance(); // 'export'
                        // export default …
        if self.is_kw("default") && self.cur_escaped() {
            return self.err("'default' must not contain escape sequences");
        }
        if self.eat_kw("default") {
            let stmt = if self.is_kw("function")
                || (self.is_ident_word("async")
                    && matches!(self.peek_kind(1), Tok::Keyword("function")))
            {
                let start = self.cur_start();
                let is_async = self.eat_ident_word("async");
                Stmt::FuncDecl(Rc::new(self.parse_function(is_async, false, start)?))
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
                if self.cur_escaped() {
                    return self.err("'as' must not contain escape sequences");
                }
                self.advance();
                Some(self.parse_module_export_name()?)
            } else {
                None
            };
            self.expect_keyword_word("from")?;
            let (source, _attr) = self.parse_module_specifier()?;
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
                    if self.cur_escaped() {
                        return self.err("'as' must not contain escape sequences");
                    }
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
                if self.cur_escaped() {
                    return self.err("'from' must not contain escape sequences");
                }
                self.advance();
                Some(self.parse_module_specifier()?.0)
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
        let mut seen_default = false;
        let mut cases = Vec::new();
        while !self.is_punct("}") && !self.at_eof() {
            let test = if self.eat_kw("case") {
                let e = self.parse_expr()?;
                Some(e)
            } else if self.eat_kw("default") {
                if seen_default {
                    self.switch_depth -= 1;
                    return self.err("more than one default clause in switch statement");
                }
                seen_default = true;
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
                self.in_case_clause = true;
                body.push(self.parse_stmt()?);
                self.in_case_clause = false;
            }
            cases.push(SwitchCase { test, body });
        }
        self.switch_depth -= 1;
        self.expect_punct("}")?;
        Ok(Stmt::Switch { disc, cases })
    }

    /// A label identifier is subject to the identifier-reference rules: `yield` is reserved in
    /// strict mode and generators, `await` in modules, async bodies and class static blocks.
    fn check_label_name(&self, name: &str) -> Result<(), ParseError> {
        if name == "yield" && (self.strict || self.in_generator) {
            return self.err("'yield' cannot be used as a label here");
        }
        // Strict-mode reserved words (and `let`) cannot label a statement.
        if self.strict
            && matches!(
                name,
                "let"
                    | "implements"
                    | "interface"
                    | "package"
                    | "private"
                    | "protected"
                    | "public"
                    | "static"
            )
        {
            return self.err(format!("'{name}' cannot be used as a label in strict mode"));
        }
        if name == "await" && (self.module || self.in_async || self.in_static_block) {
            return self.err("'await' cannot be used as a label here");
        }
        Ok(())
    }

    /// If the current token is a regex literal, re-lex its text as `/`-division followed by the
    /// pattern/flags source (used after `yield`/`await` in identifier position — see caller).
    fn split_regex_as_division(&mut self) {
        let Tok::Regex { body, flags } = self.cur().clone() else {
            return;
        };
        let tok = &self.toks[self.pos];
        let (line, start, end) = (tok.line, tok.start, tok.end);
        let rest = format!("{body}/{flags}");
        let Ok(mut relexed) = crate::lexer::tokenize(&rest) else {
            return;
        };
        for t in &mut relexed {
            t.line = line;
            t.start = start;
            t.end = end;
            t.nl_before = false;
        }
        let mut spliced = vec![Token {
            kind: Tok::Punct("/"),
            line,
            start,
            end,
            nl_before: false,
            legacy_octal: false,
            escaped: false,
            lone_surrogate: false,
        }];
        spliced.extend(relexed);
        self.toks.splice(self.pos..self.pos + 1, spliced);
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
        let mut left = self.parse_cond()?;
        let left_paren = self.last_paren;
        if let Tok::Punct(op) = self.cur() {
            let op = *op;
            if is_assign_op(op) {
                // `undefined` is an ordinary identifier as an assignment target: the write fails
                // at runtime (strict TypeError on the non-writable global), not at parse.
                if matches!(left, Expr::Undefined) {
                    left = Expr::Ident("undefined".to_string());
                }
                self.advance();
                let mut value = self.parse_assign()?;
                // NamedEvaluation applies only to an IdentifierReference target: `(x) = fn` does
                // not name the function. A transparent one-element Seq hides the anonymity.
                if matches!(op, "=" | "&&=" | "||=" | "??=")
                    && left_paren
                    && matches!(left, Expr::Ident(_))
                    && matches!(
                        &value,
                        Expr::Func(f) if f.name.is_none()
                    )
                {
                    value = Expr::Seq(vec![value]);
                }
                // Plain `=` also accepts an array/object literal reinterpreted as a destructuring
                // assignment target — but only an unparenthesized, *valid* pattern (`({}) = x`
                // stays a PrimaryExpression, which is not a target).
                let destructuring =
                    op == "=" && !left_paren && matches!(left, Expr::Array(_) | Expr::Object(_));
                if destructuring {
                    if !is_valid_assign_pattern(&left) {
                        return self.err("invalid destructuring assignment target");
                    }
                    if pattern_has_stray_cover(&left) {
                        return self.err(
                            "invalid shorthand property initializer outside a pattern position",
                        );
                    }
                    if self.strict && pattern_strict_banned(&left) {
                        return self.err("cannot assign to 'eval' or 'arguments' in strict mode");
                    }
                    // A pattern may repeat `__proto__:` — forgive dups recorded inside it.
                    self.proto_dups.truncate(proto_mark);
                } else if !is_valid_assign_target(&left) {
                    // Annex B web compat: a CallExpression is a grammatically valid target in
                    // sloppy mode; the assignment throws a ReferenceError at runtime instead.
                    // Logical assignment (&&=, ||=, ??=) has no such legacy carve-out.
                    if self.strict
                        || matches!(op, "&&=" | "||=" | "??=")
                        || !matches!(left, Expr::Call { .. })
                    {
                        return self.err("invalid assignment target");
                    }
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
                // `#field in obj` is the ergonomic brand check, not a normal `in`; the RHS
                // itself may not be a bare private name (`#f in #f in x` is a SyntaxError).
                if matches!(&right, Expr::Ident(r) if r.starts_with('#')) {
                    return self.err("a private name may only appear to the left of 'in'");
                }
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
            if self.in_static_block {
                return self.err("await is not allowed in a class static block");
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
            let mut arg = self.parse_unary()?;
            // `undefined` is an ordinary identifier reference as a delete operand (the global
            // property is non-configurable, so the delete evaluates to false).
            if op == "delete" && matches!(arg, Expr::Undefined) {
                arg = Expr::Ident("undefined".to_string());
            }
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
            // Annex B: a CallExpression operand parses in sloppy mode (runtime ReferenceError).
            Expr::Call { .. } if !self.strict => {}
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
                // `super.#x` is an early SyntaxError.
                if name.starts_with('#') && matches!(expr, Expr::Super) {
                    return self.err("cannot access a private member through 'super'");
                }
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
            let new_escaped = self.cur_escaped();
            if new_escaped {
                return self.err("'new' must not contain escape sequences");
            }
            self.advance();
            if self.eat_punct(".") {
                // `new.target` — no escape sequences, and only valid inside a function.
                if !self.is_ident_word("target") {
                    return self.err("expected 'target' after 'new.'");
                }
                if new_escaped || self.cur_escaped() {
                    return self.err("'new.target' must not contain escape sequences");
                }
                self.advance();
                if self.nonarrow_fn_depth == 0 && !self.allow_new_target {
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
                // `new import(...)` is a SyntaxError — an ImportCall (`import()`) is a
                // CallExpression, so it can't be the MemberExpression base of `new` — but a
                // parenthesized `new (import(...))` covers it into a valid PrimaryExpression.
                if !self.last_paren && callee_has_import_call(&callee) {
                    return self.err("'import(...)' cannot be used with 'new'");
                }
                // An optional chain is a CallExpression, never the MemberExpression `new` needs.
                if !self.last_paren && matches!(callee, Expr::OptionalChain(_)) {
                    return self.err("an optional chain cannot be the callee of 'new'");
                }
                let had_args = self.is_punct("(");
                let args = if had_args {
                    self.parse_args()?
                } else {
                    Vec::new()
                };
                // Without arguments this is a NewExpression, not a MemberExpression — an optional
                // chain cannot attach to it (`new C?.()` / `new o?.C()` are SyntaxErrors).
                if !had_args && self.is_punct("?.") {
                    return self.err("an optional chain cannot follow 'new' without arguments");
                }
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
                // `super.#x` is an early SyntaxError.
                if name.starts_with('#') && matches!(base, Expr::Super) {
                    return self.err("cannot access a private member through 'super'");
                }
                base = Expr::Member {
                    obj: Box::new(base),
                    prop: name,
                    optional: false,
                };
            } else if let Tok::Template(parts) = self.cur().clone() {
                // A tagged template is itself a MemberExpression: `new tag\`t\`` invokes the
                // tag and constructs its result.
                self.advance();
                base = self.build_tagged_template(base, parts)?;
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
            Tok::Ident(n) if n == "arguments" && self.no_arguments_refs => {
                self.err("'arguments' is not allowed in a class static block")
            }
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
            Tok::Keyword(k @ ("true" | "false" | "null")) => {
                // A keyword spelled with `\u` escapes is never the keyword (nor a usable
                // identifier, since the name is reserved).
                if self.cur_escaped() {
                    return self.err(format!("'{k}' must not contain escape sequences"));
                }
                self.advance();
                Ok(match k {
                    "true" => Expr::Bool(true),
                    "false" => Expr::Bool(false),
                    _ => Expr::Null,
                })
            }
            Tok::Keyword("this") => {
                self.advance();
                Ok(Expr::This)
            }
            Tok::Keyword("function") => {
                let f = self.parse_function(false, true, self.cur_start())?;
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
                } else if self.is_punct("(") {
                    // A SuperCall is only valid in a derived class constructor (or an arrow there).
                    if !self.super_call_ok {
                        return self.err("'super' call unexpected here");
                    }
                } else {
                    return self.err("'super' keyword unexpected here");
                }
                Ok(Expr::Super)
            }
            // `import(specifier)` (dynamic import), `import.meta`, or the phased forms
            // `import.source(specifier)` / `import.defer(specifier)`.
            Tok::Keyword("import") => {
                // Neither `import` in an ImportCall/ImportMeta nor the `meta` after it may be
                // spelled with escape sequences.
                if self.cur_escaped() {
                    return self.err("'import' must not contain escape sequences");
                }
                self.advance();
                if self.eat_punct(".") {
                    let phase = match self.cur() {
                        Tok::Ident(w) if w == "source" => ImportPhase::Source,
                        Tok::Ident(w) if w == "defer" => ImportPhase::Defer,
                        Tok::Ident(w) if w == "meta" => {
                            if self.cur_escaped() {
                                return self.err("'import.meta' must not contain escape sequences");
                            }
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
                    let (spec, options) = self.parse_import_call_args()?;
                    Ok(Expr::ImportCall {
                        spec: Box::new(spec),
                        phase,
                        options: options.map(Box::new),
                    })
                } else {
                    let (spec, options) = self.parse_import_call_args()?;
                    Ok(Expr::ImportCall {
                        spec: Box::new(spec),
                        phase: ImportPhase::Evaluation,
                        options: options.map(Box::new),
                    })
                }
            }
            Tok::Ident(name)
                if name == "async"
                    && !self.cur_escaped()
                    && matches!(self.peek_kind(1), Tok::Keyword("function"))
                    // No line terminator is allowed between `async` and `function`.
                    && !self
                        .toks
                        .get(self.pos + 1)
                        .map(|t| t.nl_before)
                        .unwrap_or(true) =>
            {
                let start = self.cur_start();
                self.advance();
                let f = self.parse_function(true, true, start)?;
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
                if name == "await" && (self.in_async || self.in_static_block) {
                    return self.err("'await' is not a valid identifier here");
                }
                // The lexer assumes a `/` after `yield`/`await` starts a regex (they are usually
                // keywords). When they are plain identifiers, that `/` is division: split the
                // mis-lexed regex token back into `/` + re-lexed remainder.
                if matches!(name.as_str(), "yield" | "await") {
                    self.split_regex_as_division();
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
                // A parenthesized array/object literal or assignment can never be reinterpreted
                // as a destructuring (sub)target — the wrapper blocks the pattern refinement.
                Ok(match e {
                    e @ (Expr::Array(_) | Expr::Object(_) | Expr::Assign { .. }) => {
                        Expr::Paren(P::new(e))
                    }
                    e => e,
                })
            }
            Tok::Punct("[") => {
                let e = self.parse_array();
                // An inner parenthesized element must not mark the literal itself as covered.
                self.last_paren = false;
                e
            }
            Tok::Punct("{") => {
                let e = self.parse_object();
                self.last_paren = false;
                e
            }
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
                TplPart::Str { cooked, .. } => match cooked {
                    Some(c) => Expr::Str(Rc::from(c.as_str())),
                    // An invalid escape is only legal in a *tagged* template.
                    None => {
                        return self.err("invalid escape sequence in template literal");
                    }
                },
                TplPart::Sub(src) => {
                    let tokens = crate::lexer::tokenize_goal(&src, !self.module).map_err(|e| {
                        ParseError {
                            message: e.message,
                            line: e.line,
                        }
                    })?;
                    let mut sub = Parser {
                        toks: tokens,
                        pos: 0,
                        src_chars: Rc::new(src.chars().collect()),
                        strict: self.strict,
                        depth: self.depth,
                        in_generator: self.in_generator,
                        in_async: self.in_async,
                        in_params: self.in_params,
                        no_in: false,
                        module: self.module,
                        fn_depth: self.fn_depth,
                        nonarrow_fn_depth: self.nonarrow_fn_depth,
                        iter_depth: self.iter_depth,
                        switch_depth: self.switch_depth,
                        labels: Vec::new(),
                        iter_labels: Vec::new(),
                        decl_scopes: vec![DeclScope {
                            fn_boundary: true,
                            ..Default::default()
                        }],
                        next_scope_is_fn_boundary: false,
                        allow_new_target: self.allow_new_target,
                        top_level: false,
                        super_prop_ok: self.super_prop_ok,
                        super_call_ok: self.super_call_ok,
                        in_derived_class: false,
                        in_case_clause: false,
                        no_arguments_refs: false,
                        proto_dups: Vec::new(),
                        last_paren: false,
                        single_stmt: false,
                        in_static_block: false,
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
                TplPart::Str { cooked, raw } => quasis.push((cooked, raw)),
                TplPart::Sub(src) => {
                    let tokens = crate::lexer::tokenize_goal(&src, !self.module).map_err(|e| {
                        ParseError {
                            message: e.message,
                            line: e.line,
                        }
                    })?;
                    let mut sub = Parser {
                        toks: tokens,
                        pos: 0,
                        src_chars: Rc::new(src.chars().collect()),
                        strict: self.strict,
                        depth: self.depth,
                        in_generator: self.in_generator,
                        in_async: self.in_async,
                        in_params: self.in_params,
                        no_in: false,
                        module: self.module,
                        fn_depth: self.fn_depth,
                        nonarrow_fn_depth: self.nonarrow_fn_depth,
                        iter_depth: self.iter_depth,
                        switch_depth: self.switch_depth,
                        labels: Vec::new(),
                        iter_labels: Vec::new(),
                        decl_scopes: vec![DeclScope {
                            fn_boundary: true,
                            ..Default::default()
                        }],
                        next_scope_is_fn_boundary: false,
                        allow_new_target: self.allow_new_target,
                        top_level: false,
                        super_prop_ok: self.super_prop_ok,
                        super_call_ok: self.super_call_ok,
                        in_derived_class: false,
                        in_case_clause: false,
                        no_arguments_refs: false,
                        proto_dups: Vec::new(),
                        last_paren: false,
                        single_stmt: false,
                        in_static_block: false,
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
                let inner = self.parse_assign()?;
                // A spread followed by a comma is a valid literal but can't be a rest element in
                // a destructuring pattern; the transparent Seq wrapper marks that for validation.
                let followed = self.is_punct(",");
                elems.push(ArrayElem::Spread(if followed {
                    Expr::Seq(vec![inner])
                } else {
                    inner
                }));
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

    /// A private name is only a valid member name inside a class body, never an object literal.
    fn reject_private_key(&self, key: &PropKey) -> Result<(), ParseError> {
        if let PropKey::Ident(n) = key {
            if n.starts_with('#') {
                return self.err("private names are only allowed in class bodies");
            }
        }
        Ok(())
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
            // The property-definition start: a method's `toString` source begins here.
            let member_start = self.cur_start();
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
                self.reject_private_key(&key)?;
                let func = self.parse_accessor_function(is_get, member_start)?;
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
                // `async` must be followed by the method name on the same line.
                if self.nl_before() && !self.is_punct("*") {
                    return self.err("line terminator not allowed after 'async' in a method");
                }
            }
            let is_generator = self.eat_punct("*");
            let key = self.parse_prop_key()?;
            self.reject_private_key(&key)?;
            if (is_async || is_generator) && !self.is_punct("(") {
                return self.err("expected a method after 'async'/'*' in an object literal");
            }
            if self.is_punct("(") {
                // Method shorthand.
                let func = if is_async || is_generator {
                    self.parse_method_function_kind(is_generator, is_async, member_start)?
                } else {
                    self.parse_method_function(member_start)?
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
                            // CoverInitializedName: only valid when the literal is reinterpreted
                            // as a destructuring pattern; as a plain literal it is a SyntaxError
                            // (deferred exactly like duplicate `__proto__`).
                            self.proto_dups.push(ParseError {
                                message: "invalid shorthand property initializer".into(),
                                line: self.line(),
                            });
                            let default = self.parse_assign()?;
                            let value = Expr::Assign {
                                op: "=",
                                target: Box::new(ident),
                                value: Box::new(default),
                            };
                            props.push(PropDef::Cover { key, value });
                            if !self.eat_punct(",") {
                                break;
                            }
                            continue;
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

    fn parse_function(
        &mut self,
        is_async: bool,
        is_expr: bool,
        start: u32,
    ) -> Result<Function, ParseError> {
        self.eat_kw("function");
        let is_generator = self.eat_punct("*");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            // A *declaration*'s name is a BindingIdentifier in the enclosing context: `await`
            // is reserved in async bodies / static blocks / modules, `yield` in generators.
            // (An expression's name binds inside the function, past those boundaries.)
            if !is_expr
                && ((n == "await" && (self.in_async || self.in_static_block || self.module))
                    || (n == "yield" && (self.in_generator || self.strict)))
            {
                return self.err(format!("'{n}' cannot be used as a function name here"));
            }
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
        // from here, correctly seeing no super) — and it leaves a static block's await reservation.
        let ssuper = std::mem::replace(&mut self.super_prop_ok, false);
        let scall = std::mem::replace(&mut self.super_call_ok, false);
        let ssb = std::mem::replace(&mut self.in_static_block, false);
        let sargs = std::mem::replace(&mut self.no_arguments_refs, false);
        let params = self.parse_params_fn()?;
        let (body, is_strict) = self.parse_function_body(!params_complex(&params), false)?;
        self.super_prop_ok = ssuper;
        self.super_call_ok = scall;
        self.in_static_block = ssb;
        self.no_arguments_refs = sargs;
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
            source: self.src_slice(start, self.prev_end()),
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
        let class_start = self.cur_start();
        let decorators = self.parse_decorators()?;
        self.eat_kw("class");
        let name = if let Tok::Ident(n) = self.cur().clone() {
            self.advance();
            // A class definition is always strict, so its name can't be a reserved word;
            // `await` is additionally reserved in modules, async bodies and static blocks.
            if is_strict_reserved_binding(&n) {
                return self.err(format!("'{n}' cannot be used as a class name"));
            }
            if n == "await" && (self.module || self.in_async || self.in_static_block) {
                return self.err("'await' cannot be used as a class name here");
            }
            Some(n)
        } else {
            None
        };
        // All class code — the heritage clause included — is strict mode.
        let saved = self.strict;
        self.strict = true;
        let superclass = if self.eat_kw("extends") {
            let sc = self.parse_lhs();
            if sc.is_err() {
                self.strict = saved;
            }
            Some(Box::new(sc?))
        } else {
            None
        };
        self.expect_punct("{")?;
        let sderived = std::mem::replace(&mut self.in_derived_class, superclass.is_some());
        let mut members = Vec::new();
        while !self.is_punct("}") && !self.at_eof() {
            members.extend(self.parse_class_member()?);
        }
        self.in_derived_class = sderived;
        self.strict = saved;
        self.expect_punct("}")?;
        // The class constructor's `toString` is the whole class's source text.
        let class_src = self.src_slice(class_start, self.prev_end());
        for m in &mut members {
            if matches!(m.kind, MemberKind::Constructor) {
                if let Some(f) = &mut m.func {
                    if let Some(fm) = Rc::get_mut(f) {
                        fm.source = class_src.clone();
                    }
                }
            }
        }
        validate_class(&members).map_err(|m| ParseError {
            message: m,
            line: self.line(),
        })?;
        Ok(Class {
            name,
            superclass,
            members,
            decorators,
            source: self.src_slice(class_start, self.prev_end()),
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
        // (get/set before a `*` is a *field* named get/set followed by a generator method.)
        if is_static && self.is_punct("{") {
            self.advance();
            let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
            let scall = std::mem::replace(&mut self.super_call_ok, false);
            let snt = std::mem::replace(&mut self.allow_new_target, true);
            let ssb = std::mem::replace(&mut self.in_static_block, true);
            let sargs = std::mem::replace(&mut self.no_arguments_refs, true);
            // The block is function-like code: not generator/async, and a
            // break/continue/label boundary.
            let sg = std::mem::take(&mut self.in_generator);
            let sa = std::mem::take(&mut self.in_async);
            let sid = std::mem::take(&mut self.iter_depth);
            let ssd = std::mem::take(&mut self.switch_depth);
            let slabels = std::mem::take(&mut self.labels);
            let sil = std::mem::take(&mut self.iter_labels);
            let body = self.parse_block_body();
            self.in_generator = sg;
            self.in_async = sa;
            self.iter_depth = sid;
            self.switch_depth = ssd;
            self.labels = slabels;
            self.iter_labels = sil;
            self.super_prop_ok = ssuper;
            self.super_call_ok = scall;
            self.allow_new_target = snt;
            self.in_static_block = ssb;
            self.no_arguments_refs = sargs;
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
                source: None,
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
        // The MethodDefinition start (excludes `static`): `toString` source begins here.
        let member_start = self.cur_start();
        let mut kind = MemberKind::Method;
        if (self.is_ident_word("get") || self.is_ident_word("set"))
            && !self.cur_escaped()
            && !self.next_is_member_terminator(1)
            // `get` followed by `*` is a *field* named get and a generator method (ASI).
            && !matches!(self.peek_kind(1), Tok::Punct("*"))
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
            // Only a derived class's `constructor` body is a SuperCall context.
            let is_ctor = kind == MemberKind::Method && !is_static && key_is(&key, "constructor");
            let scall =
                std::mem::replace(&mut self.super_call_ok, is_ctor && self.in_derived_class);
            let func = self.parse_method_function_kind(is_generator, is_async, member_start);
            self.super_call_ok = scall;
            let mut func = func?;
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
                let scall = std::mem::replace(&mut self.super_call_ok, false);
                let snt = std::mem::replace(&mut self.allow_new_target, true);
                let v = self.parse_assign();
                self.super_prop_ok = ssuper;
                self.super_call_ok = scall;
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

    fn parse_method_function(&mut self, start: u32) -> Result<Function, ParseError> {
        self.parse_method_function_kind(false, false, start)
    }

    fn parse_method_function_kind(
        &mut self,
        is_generator: bool,
        is_async: bool,
        start: u32,
    ) -> Result<Function, ParseError> {
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        // A method body (and any parameter default) is a super-property context. Whether it is a
        // SuperCall context was decided by the caller (only a derived class constructor is).
        let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
        // A method body is a function boundary: it leaves a static block's `await` and
        // `arguments` reservations.
        let ssb = std::mem::replace(&mut self.in_static_block, false);
        let sargs = std::mem::replace(&mut self.no_arguments_refs, false);
        let params = self.parse_params_fn()?;
        // A method has UniqueFormalParameters: duplicate parameter names are always an error.
        if let Some(dup) = duplicate_name(&param_names(&params)) {
            self.in_generator = sg;
            self.in_async = sa;
            self.super_prop_ok = ssuper;
            self.in_static_block = ssb;
            self.no_arguments_refs = sargs;
            return self.err(format!("duplicate parameter name '{dup}'"));
        }
        let (body, is_strict) = self.parse_function_body(!params_complex(&params), false)?;
        self.super_prop_ok = ssuper;
        self.in_static_block = ssb;
        self.no_arguments_refs = sargs;
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
            source: self.src_slice(start, self.prev_end()),
        })
    }

    fn parse_accessor_function(
        &mut self,
        is_get: bool,
        start: u32,
    ) -> Result<Function, ParseError> {
        let f = self.parse_method_function(start)?;
        check_accessor_arity(&f, is_get).map_err(|m| ParseError {
            message: m,
            line: self.line(),
        })?;
        // A "use strict" body subjects the parameter names to the strict binding rules.
        if f.is_strict && !self.strict {
            for n in param_names(&f.params) {
                if is_strict_reserved_binding(&n) {
                    return self.err(format!("'{n}' cannot be used as a binding in strict mode"));
                }
            }
        }
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

    /// Parameters of a non-arrow function: default expressions are inside the function for
    /// `new.target` purposes (an arrow's parameters instead see the enclosing context).
    fn parse_params_fn(&mut self) -> Result<Vec<Param>, ParseError> {
        self.fn_depth += 1;
        self.nonarrow_fn_depth += 1;
        let r = self.parse_params();
        self.fn_depth -= 1;
        self.nonarrow_fn_depth -= 1;
        r
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
        is_arrow: bool,
    ) -> Result<(Vec<Stmt>, bool), ParseError> {
        self.expect_punct("{")?;
        let saved_strict = self.strict;
        let inner_strict = self.prologue_has_use_strict(self.pos);
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
        if !is_arrow {
            self.nonarrow_fn_depth += 1;
        }
        self.iter_depth = 0;
        self.switch_depth = 0;
        self.next_scope_is_fn_boundary = true;
        let body = self.parse_block_body();
        self.fn_depth -= 1;
        if !is_arrow {
            self.nonarrow_fn_depth -= 1;
        }
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
        let arrow_start = self.cur_start();
        // Optional `async` prefix (on the same line) for an async arrow.
        let async_arrow = self.is_ident_word("async")
            && !self.cur_escaped()
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
                                // The lone BindingIdentifier is validated like any binding: strict-reserved
                                // words (arrow bodies inherit strictness), `await` in async / static-block
                                // contexts, `yield` in generators.
                if self.strict && is_strict_reserved_binding(&name) {
                    return self.err(format!(
                        "'{name}' cannot be used as a binding in strict mode"
                    ));
                }
                if ((async_arrow || self.in_async || self.in_static_block) && name == "await")
                    || (self.in_generator && name == "yield")
                {
                    return self.err(format!("'{name}' cannot be used as a binding here"));
                }
                let params = vec![Param {
                    pattern: Pattern::Ident(name),
                    default: None,
                    rest: false,
                }];
                return Ok(Some(self.finish_arrow(params, async_arrow, arrow_start)?));
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
                    // An async arrow's parameters are [+Await]: `await` is reserved there.
                    let sa = self.in_async;
                    self.in_async = sa || async_arrow;
                    let params = self.parse_params();
                    self.in_async = sa;
                    let params = params?;
                    self.expect_punct("=>")?;
                    return Ok(Some(self.finish_arrow(params, async_arrow, arrow_start)?));
                }
            }
        }
        Ok(None)
    }

    fn finish_arrow(
        &mut self,
        params: Vec<Param>,
        is_async: bool,
        start: u32,
    ) -> Result<Expr, ParseError> {
        // Arrow parameters must always be unique (the list is treated as if it had a `[+Strict]`).
        if let Some(dup) = duplicate_name(&param_names(&params)) {
            return self.err(format!("duplicate parameter name '{dup}'"));
        }
        let sa = self.in_async;
        self.in_async = is_async;
        // An arrow body is a function boundary for a static block's `await` reservation
        // (the parameters, parsed by the caller, are not).
        let ssb = std::mem::replace(&mut self.in_static_block, false);
        let result = if self.is_punct("{") {
            let (body, is_strict) = self.parse_function_body(!params_complex(&params), true)?;
            // A body-level lexical may not redeclare a parameter name.
            if let Some(dup) = params_body_lexical_clash(&params, &body) {
                self.in_async = sa;
                self.in_static_block = ssb;
                return self.err(format!("Identifier '{dup}' has already been declared"));
            }
            // A "use strict" body subjects the parameter names to the strict binding rules.
            if is_strict && !self.strict {
                for n in param_names(&params) {
                    if is_strict_reserved_binding(&n) {
                        return self
                            .err(format!("'{n}' cannot be used as a binding in strict mode"));
                    }
                }
            }
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
                source: self.src_slice(start, self.prev_end()),
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
                source: self.src_slice(start, self.prev_end()),
            }
        };
        self.in_async = sa;
        self.in_static_block = ssb;
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
/// pattern (see `expr_to_pattern`), an object `AssignmentRestProperty` target may be any simple
/// `LeftHandSideExpression` (e.g. a member expression) — never a nested pattern — and
/// element/property targets may be member expressions too.
fn valid_assignment_pattern(e: &Expr) -> bool {
    fn simple(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Ident(_)
                | Expr::Member {
                    optional: false,
                    ..
                }
                | Expr::Index {
                    optional: false,
                    ..
                }
        )
    }
    fn target_ok(e: &Expr) -> bool {
        simple(e) || valid_assignment_pattern(e)
    }
    match e {
        Expr::Array(elems) => elems.iter().enumerate().all(|(idx, el)| match el {
            ArrayElem::Hole => true,
            ArrayElem::Spread(t) => {
                // A rest element must be last (the parser's Seq wrapper marks one followed
                // by a comma) and can't carry a default.
                idx == elems.len() - 1
                    && !matches!(t, Expr::Seq(_) | Expr::Assign { op: "=", .. })
                    && target_ok(t)
            }
            ArrayElem::Item(Expr::Assign {
                op: "=", target, ..
            }) => target_ok(target),
            ArrayElem::Item(t) => target_ok(t),
        }),
        Expr::Object(props) => props.iter().enumerate().all(|(idx, p)| match p {
            PropDef::KeyValue { value, .. }
            | PropDef::Cover { value, .. }
            | PropDef::Proto(value) => match value {
                Expr::Assign {
                    op: "=", target, ..
                } => target_ok(target),
                v => target_ok(v),
            },
            // AssignmentRestProperty: last, and a simple target (never a nested pattern).
            PropDef::Spread(t) => idx == props.len() - 1 && simple(t),
            _ => false,
        }),
        _ => false,
    }
}

/// Whether a destructuring assignment pattern targets `eval`/`arguments` (a strict-mode
/// SyntaxError). Walks target positions only, not keys or defaults.
fn pattern_strict_banned(e: &Expr) -> bool {
    let banned = |n: &str| n == "eval" || n == "arguments";
    match e {
        Expr::Ident(n) => banned(n),
        Expr::Array(elems) => elems.iter().any(|el| match el {
            ArrayElem::Hole => false,
            ArrayElem::Spread(t) => pattern_strict_banned(t),
            ArrayElem::Item(Expr::Assign {
                op: "=", target, ..
            }) => pattern_strict_banned(target),
            ArrayElem::Item(t) => pattern_strict_banned(t),
        }),
        Expr::Object(props) => props.iter().any(|p| match p {
            PropDef::KeyValue {
                value: Expr::Assign {
                    op: "=", target, ..
                },
                ..
            }
            | PropDef::Cover {
                value: Expr::Assign {
                    op: "=", target, ..
                },
                ..
            } => pattern_strict_banned(target),
            PropDef::KeyValue { value, .. } => pattern_strict_banned(value),
            PropDef::Proto(Expr::Assign {
                op: "=", target, ..
            }) => pattern_strict_banned(target),
            PropDef::Proto(v) => pattern_strict_banned(v),
            PropDef::Spread(t) => pattern_strict_banned(t),
            _ => false,
        }),
        _ => false,
    }
}

/// Whether a reinterpreted assignment pattern contains a CoverInitializedName in a
/// *non-pattern* position (e.g. the member base in `[{a = 0}.x] = []`) — a SyntaxError the
/// blanket cover-forgiveness must not absorb.
fn pattern_has_stray_cover(e: &Expr) -> bool {
    // Generic-expression positions: any object literal with a Cover prop (at any depth) is bad.
    fn generic(e: &Expr) -> bool {
        crate::eval::expr_contains(e, |x| {
            matches!(x, Expr::Object(props)
                if props.iter().any(|p| matches!(p, PropDef::Cover { .. })))
        })
    }
    fn target(t: &Expr) -> bool {
        match t {
            Expr::Array(_) | Expr::Object(_) => pattern_has_stray_cover(t),
            other => generic(other),
        }
    }
    match e {
        Expr::Array(elems) => elems.iter().any(|el| match el {
            ArrayElem::Hole => false,
            ArrayElem::Spread(t) => target(t),
            ArrayElem::Item(Expr::Assign {
                op: "=",
                target: t,
                value,
            }) => target(t) || generic(value),
            ArrayElem::Item(t) => target(t),
        }),
        Expr::Object(props) => props.iter().any(|p| match p {
            PropDef::KeyValue {
                value:
                    Expr::Assign {
                        op: "=",
                        target: t,
                        value,
                    },
                ..
            }
            | PropDef::Cover {
                value:
                    Expr::Assign {
                        op: "=",
                        target: t,
                        value,
                    },
                ..
            }
            | PropDef::Proto(Expr::Assign {
                op: "=",
                target: t,
                value,
            }) => target(t) || generic(value),
            PropDef::KeyValue { value, .. } | PropDef::Proto(value) => target(value),
            PropDef::Spread(t) => target(t),
            _ => false,
        }),
        _ => false,
    }
}

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
            }
            | PropDef::Cover {
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
                    PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
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
        // A bare private name is never an assignment target.
        Expr::Ident(name) if name.starts_with('#') => None,
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
                    PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
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
