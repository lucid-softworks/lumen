//! The abstract syntax tree. Deliberately small: one `Stmt` enum and one `Expr` enum, with shared
//! sub-structures for functions and patterns. The interpreter walks this tree directly.

use std::rc::Rc;

pub type P<T> = Box<T>;

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    /// `var` / `let` / `const` declaration: kind + (target, optional initializer) pairs.
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    FuncDecl(Rc<Function>),
    Return(Option<Expr>),
    If {
        test: Expr,
        cons: P<Stmt>,
        alt: Option<P<Stmt>>,
    },
    Block(Vec<Stmt>),
    While {
        test: Expr,
        body: P<Stmt>,
    },
    DoWhile {
        body: P<Stmt>,
        test: Expr,
    },
    /// C-style `for (init; test; update) body`.
    For {
        init: Option<P<ForInit>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: P<Stmt>,
    },
    /// `for (left in right) body` / `for (left of right) body` (`is_await` for `for await … of`).
    ForInOf {
        decl: Option<DeclKind>,
        left: Pattern,
        right: Expr,
        of: bool,
        is_await: bool,
        body: P<Stmt>,
    },
    Break(Option<String>),
    Continue(Option<String>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        handler: Option<(Option<Pattern>, Vec<Stmt>)>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    Labeled {
        label: String,
        body: P<Stmt>,
    },
    /// `with (obj) body` — resolves identifiers against `obj` first (forbidden in strict mode).
    With {
        obj: Expr,
        body: P<Stmt>,
    },
    ClassDecl(Rc<Class>),
    Empty,
    Debugger,
    /// `import …from "spec"` (or a bare `import "spec"`).
    Import(ImportDecl),
    /// `export { a, b as c }` or `export { a } from "spec"`.
    ExportNamed {
        specs: Vec<ExportSpec>,
        source: Option<Rc<str>>,
    },
    /// `export const/let/var/function/class …` — the inner declaration plus its exported names.
    ExportDecl(P<Stmt>),
    /// `export default …` (expression, function, or class).
    ExportDefault(P<Stmt>),
    /// `export * from "spec"` or `export * as ns from "spec"`.
    ExportAll {
        source: Rc<str>,
        exported: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub source: Rc<str>,
    pub specs: Vec<ImportSpec>,
    /// The `with { type: "..." }` import attribute (json/text/bytes), if present.
    pub attr_type: Option<String>,
}
#[derive(Debug, Clone)]
pub enum ImportSpec {
    /// `import x from "…"`
    Default(String),
    /// `import * as ns from "…"`
    Namespace(String),
    /// `import defer * as ns from "…"` — evaluation deferred until the namespace is accessed.
    DeferNamespace(String),
    /// `import source x from "…"` — a source-phase import binding the module's ModuleSource.
    Source(String),
    /// `import { imported as local } from "…"`
    Named { imported: String, local: String },
}
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub local: String,
    pub exported: String,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: Option<String>,
    pub superclass: Option<P<Expr>>,
    pub members: Vec<ClassMember>,
    /// `@dec` decorators applied to the whole class (outermost last).
    pub decorators: Vec<Expr>,
    /// The class's source text (what the constructor's `toString` returns).
    pub source: Option<Rc<str>>,
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub key: PropKey,
    pub kind: MemberKind,
    pub is_static: bool,
    /// For methods/accessors/constructor.
    pub func: Option<Rc<Function>>,
    /// For fields (`x = init` / `x`).
    pub value: Option<Expr>,
    /// `@dec` decorators applied to this element.
    pub decorators: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Constructor,
    Method,
    Get,
    Set,
    Field,
    /// `accessor x = init` — an auto-accessor: a private backing field plus a getter/setter pair.
    Accessor,
    /// `static { ... }` — runs once at class definition with `this` = the class.
    StaticBlock,
}

#[derive(Debug, Clone)]
pub enum ForInit {
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// `None` is the `default:` clause.
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
    /// `using x = expr;` — a block-scoped binding disposed (`[Symbol.dispose]()`) at scope exit.
    Using,
    /// `await using x = expr;` — disposed via `[Symbol.asyncDispose]()` (awaited) at scope exit.
    AwaitUsing,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    /// `[a, b = 1, ...rest]` — elements may be holes, may carry defaults, and the last may be a rest.
    Array(Vec<ArrayPatElem>),
    /// `{ a, b: x = 1, ...rest }`.
    Object(ObjectPat),
    /// A member-expression assignment target (`o.p`, `o[k]`) — only valid in assignment-style
    /// destructuring / `for (o.p of …)`, never in a declaration.
    Member(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum ArrayPatElem {
    Hole,
    Elem {
        pattern: Pattern,
        default: Option<Expr>,
    },
    Rest(Pattern),
}

#[derive(Debug, Clone)]
pub struct ObjectPat {
    pub props: Vec<ObjPatProp>,
    /// `...rest` — a plain identifier collecting the remaining own enumerable keys.
    pub rest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObjPatProp {
    pub key: PropKey,
    pub value: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // some node fields (regex body/flags) are parsed before they are interpreted
pub enum Expr {
    /// A parenthesized array/object literal or assignment — recorded so destructuring
    /// reinterpretation can reject it (parens block the pattern refinement); evaluates
    /// transparently.
    Paren(P<Expr>),
    Num(f64),
    BigInt(crate::bigint::JsBigInt),
    Str(Rc<str>),
    /// A template-literal substitution: evaluate the inner expression and apply ToString (which uses
    /// the `string` hint — toString before valueOf — unlike `+` which uses the `default` hint).
    ToStr(Box<Expr>),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    This,
    Regex {
        body: Rc<str>,
        flags: Rc<str>,
    },
    Array(Vec<ArrayElem>),
    Object(Vec<PropDef>),
    Func(Rc<Function>),
    Class(Rc<Class>),
    /// `yield expr` / `yield* expr` (only inside a generator).
    Yield {
        delegate: bool,
        arg: Option<P<Expr>>,
    },
    /// `await expr` (only inside an async function).
    Await(P<Expr>),
    /// The bare `super` keyword (only valid as `super(...)` or `super.x` / `super[x]`).
    Super,
    Unary {
        op: &'static str,
        arg: P<Expr>,
    },
    Update {
        op: &'static str,
        prefix: bool,
        arg: P<Expr>,
    },
    Binary {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Logical {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Assign {
        op: &'static str,
        target: P<Expr>,
        value: P<Expr>,
    },
    Cond {
        test: P<Expr>,
        cons: P<Expr>,
        alt: P<Expr>,
    },
    Call {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
        optional: bool,
    },
    New {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
    },
    Member {
        obj: P<Expr>,
        prop: String,
        optional: bool,
    },
    Index {
        obj: P<Expr>,
        index: P<Expr>,
        optional: bool,
    },
    Seq(Vec<Expr>),
    /// `tag\`a${x}b\`` — `quasis` are (cooked, raw) chunks (one more than `subs`).
    TaggedTemplate {
        tag: P<Expr>,
        quasis: Vec<(Option<String>, String)>,
        subs: Vec<Expr>,
    },
    /// An optional chain (`a?.b.c`): evaluates the inner LHS, short-circuiting to `undefined` if any
    /// `?.` link sees a nullish base.
    OptionalChain(P<Expr>),
    /// Ergonomic brand check `#field in obj`: whether `obj` carries the private field.
    PrivateIn {
        name: String,
        obj: P<Expr>,
    },
    /// Dynamic `import(specifier)` / `import.source(...)` / `import.defer(...)` — returns a
    /// promise. The phase selects the import semantics.
    ImportCall {
        spec: P<Expr>,
        phase: ImportPhase,
        /// The optional second argument (`import(spec, { with: { type: "json" } })`).
        options: Option<P<Expr>>,
    },
    /// `import.meta`.
    ImportMeta,
    /// `new.target`.
    NewTarget,
}

/// The phase of a dynamic `import()` call: plain evaluation, `import.source(...)` (source-phase),
/// or `import.defer(...)` (deferred-evaluation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPhase {
    Evaluation,
    Source,
    Defer,
}

/// An array element or call argument: a value, a spread (`...x`), or a hole (`[1,,3]`).
#[derive(Debug, Clone)]
pub enum ArrayElem {
    Item(Expr),
    Spread(Expr),
    Hole,
}

#[derive(Debug, Clone)]
pub enum PropDef {
    /// `key: value` or shorthand `{ x }`.
    KeyValue {
        key: PropKey,
        value: Expr,
    },
    /// CoverInitializedName (`{ x = default }`): only valid when the literal is reinterpreted as
    /// a destructuring pattern; the parser rejects it anywhere else. `value` is the
    /// `x = default` assignment.
    Cover {
        key: PropKey,
        value: Expr,
    },
    /// Concise method `key() {}` (incl. generator/async). Carries a [[HomeObject]] so `super`
    /// inside the body resolves against the literal's prototype.
    Method {
        key: PropKey,
        func: Rc<Function>,
    },
    /// `get key() {}` / `set key(v) {}`.
    Getter {
        key: PropKey,
        func: Rc<Function>,
    },
    Setter {
        key: PropKey,
        func: Rc<Function>,
    },
    Spread(Expr),
    /// The colon-form `__proto__: value` in an object literal — sets `[[Prototype]]` (when the
    /// value is an Object or Null) rather than creating a property. Only the non-computed,
    /// non-shorthand, non-method form. As a destructuring pattern it degrades to a normal
    /// `__proto__` keyed target.
    Proto(Expr),
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Ident(String),
    Str(Rc<str>),
    Num(f64),
    Computed(Expr),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // expr_body is recorded for a future `toString`/source-fidelity pass
pub struct Function {
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub is_arrow: bool,
    pub is_strict: bool,
    /// Arrow with an expression body (`x => x+1`): the single statement is a synthetic `return`.
    pub expr_body: bool,
    pub is_generator: bool,
    pub is_async: bool,
    /// A concise method / getter / setter: has no own `prototype` and is not a constructor (the
    /// class `constructor` member is re-flagged false once identified).
    pub is_method: bool,
    /// A function *expression* (`(function f(){})`): its own name binds immutably inside the
    /// function. A declaration's name binds (mutably) in the enclosing scope instead.
    pub is_fn_expr: bool,
    /// The source text this function was parsed from, for `Function.prototype.toString`.
    pub source: Option<Rc<str>>,
    /// Lazily-computed body facts (see [`Function::scan_flags`]): bit 0 = scanned, bit 1 =
    /// references `arguments`, bit 2 = references `new.target`, bit 3 = references `this`.
    /// A direct `eval` sets all three (it can reach any of them dynamically).
    pub scan: std::cell::Cell<u8>,
}

pub const SCAN_DONE: u8 = 1;
pub const SCAN_ARGUMENTS: u8 = 2;
pub const SCAN_NEW_TARGET: u8 = 4;
pub const SCAN_THIS: u8 = 8;

impl Function {
    /// What this function's own activation must provide: whether the body (or a nested arrow, or a
    /// possible direct `eval`) can observe `arguments`, `new.target`, or `this`. Ordinary nested
    /// functions are opaque (they get their own); arrows are transparent. Conservative on the
    /// safe side: a false positive only costs an unused binding.
    pub fn scan_flags(&self) -> u8 {
        let cached = self.scan.get();
        if cached & SCAN_DONE != 0 {
            return cached;
        }
        let mut flags = SCAN_DONE;
        for p in &self.params {
            scan_pattern(&p.pattern, &mut flags);
            if let Some(d) = &p.default {
                scan_expr(d, &mut flags);
            }
        }
        scan_stmts(&self.body, &mut flags);
        self.scan.set(flags);
        flags
    }
}

const SCAN_ALL: u8 = SCAN_DONE | SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS;

fn scan_stmts(body: &[Stmt], flags: &mut u8) {
    for s in body {
        scan_stmt(s, flags);
        if *flags == SCAN_ALL {
            return;
        }
    }
}

fn scan_stmt(s: &Stmt, flags: &mut u8) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => scan_expr(e, flags),
        Stmt::VarDecl { kind: _, decls } => {
            for (pat, init) in decls {
                scan_pattern(pat, flags);
                if let Some(e) = init {
                    scan_expr(e, flags);
                }
            }
        }
        // A nested (non-arrow) function has its own arguments/new.target/this.
        Stmt::FuncDecl(_) => {}
        Stmt::Return(e) => {
            if let Some(e) = e {
                scan_expr(e, flags);
            }
        }
        Stmt::If { test, cons, alt } => {
            scan_expr(test, flags);
            scan_stmt(cons, flags);
            if let Some(a) = alt {
                scan_stmt(a, flags);
            }
        }
        Stmt::Block(b) => scan_stmts(b, flags),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            scan_expr(test, flags);
            scan_stmt(body, flags);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            match init.as_deref() {
                Some(ForInit::VarDecl { kind: _, decls }) => {
                    for (pat, e) in decls {
                        scan_pattern(pat, flags);
                        if let Some(e) = e {
                            scan_expr(e, flags);
                        }
                    }
                }
                Some(ForInit::Expr(e)) => scan_expr(e, flags),
                None => {}
            }
            if let Some(e) = test {
                scan_expr(e, flags);
            }
            if let Some(e) = update {
                scan_expr(e, flags);
            }
            scan_stmt(body, flags);
        }
        Stmt::ForInOf {
            decl: _,
            left,
            right,
            of: _,
            is_await: _,
            body,
        } => {
            scan_pattern(left, flags);
            scan_expr(right, flags);
            scan_stmt(body, flags);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => {}
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            scan_stmts(block, flags);
            if let Some((param, hbody)) = handler {
                if let Some(p) = param {
                    scan_pattern(p, flags);
                }
                scan_stmts(hbody, flags);
            }
            if let Some(f) = finalizer {
                scan_stmts(f, flags);
            }
        }
        Stmt::Switch { disc, cases } => {
            scan_expr(disc, flags);
            for c in cases {
                if let Some(t) = &c.test {
                    scan_expr(t, flags);
                }
                scan_stmts(&c.body, flags);
            }
        }
        Stmt::Labeled { label: _, body } => scan_stmt(body, flags),
        Stmt::With { obj, body } => {
            scan_expr(obj, flags);
            scan_stmt(body, flags);
        }
        Stmt::ClassDecl(c) => scan_class(c, flags),
        Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => {}
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => scan_stmt(inner, flags),
    }
}

fn scan_expr(e: &Expr, flags: &mut u8) {
    match e {
        Expr::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Expr::This => *flags |= SCAN_THIS,
        Expr::NewTarget => *flags |= SCAN_NEW_TARGET,
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Regex { .. }
        | Expr::ImportMeta => {}
        // `super.x` resolves its receiver through the `this` binding; `super()` initializes it.
        Expr::Super => *flags |= SCAN_THIS,
        Expr::Paren(inner)
        | Expr::ToStr(inner)
        | Expr::Await(inner)
        | Expr::OptionalChain(inner) => scan_expr(inner, flags),
        Expr::Array(elems) => {
            for el in elems {
                match el {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Object(props) => {
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                        scan_prop_key(key, flags);
                        scan_expr(value, flags);
                    }
                    // A concise method/accessor body is its own function scope; only its
                    // (computed) key evaluates here.
                    PropDef::Method { key, func: _ }
                    | PropDef::Getter { key, func: _ }
                    | PropDef::Setter { key, func: _ } => scan_prop_key(key, flags),
                    PropDef::Spread(e) | PropDef::Proto(e) => scan_expr(e, flags),
                }
            }
        }
        // An arrow is transparent (it closes over the enclosing activation); an ordinary
        // function expression is opaque.
        Expr::Func(f) => {
            if f.is_arrow {
                let inner = f.scan_flags();
                *flags |= inner & (SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS);
            }
        }
        Expr::Class(c) => scan_class(c, flags),
        Expr::Yield { delegate: _, arg } => {
            if let Some(a) = arg {
                scan_expr(a, flags);
            }
        }
        Expr::Unary { op: _, arg }
        | Expr::Update {
            op: _,
            prefix: _,
            arg,
        } => scan_expr(arg, flags),
        Expr::Binary { op: _, left, right } | Expr::Logical { op: _, left, right } => {
            scan_expr(left, flags);
            scan_expr(right, flags);
        }
        Expr::Assign {
            op: _,
            target,
            value,
        } => {
            scan_expr(target, flags);
            scan_expr(value, flags);
        }
        Expr::Cond { test, cons, alt } => {
            scan_expr(test, flags);
            scan_expr(cons, flags);
            scan_expr(alt, flags);
        }
        Expr::Call {
            callee,
            args,
            optional: _,
        } => {
            // A direct `eval` can name any of the three dynamically.
            if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                *flags |= SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS;
            }
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::New { callee, args } => {
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Member {
            obj,
            prop: _,
            optional: _,
        } => scan_expr(obj, flags),
        Expr::Index {
            obj,
            index,
            optional: _,
        } => {
            scan_expr(obj, flags);
            scan_expr(index, flags);
        }
        Expr::Seq(exprs) => {
            for e in exprs {
                scan_expr(e, flags);
            }
        }
        Expr::TaggedTemplate {
            tag,
            quasis: _,
            subs,
        } => {
            scan_expr(tag, flags);
            for e in subs {
                scan_expr(e, flags);
            }
        }
        Expr::PrivateIn { name: _, obj } => scan_expr(obj, flags),
        Expr::ImportCall {
            spec,
            phase: _,
            options,
        } => {
            scan_expr(spec, flags);
            if let Some(o) = options {
                scan_expr(o, flags);
            }
        }
    }
}

fn scan_class(c: &Class, flags: &mut u8) {
    // Heritage, decorators, and computed keys evaluate in the enclosing scope. Member bodies are
    // their own function scopes; field/accessor initializers and static blocks can't legally name
    // `arguments`, but walking them costs only a possible false positive.
    if let Some(sc) = &c.superclass {
        scan_expr(sc, flags);
    }
    for d in &c.decorators {
        scan_expr(d, flags);
    }
    for m in &c.members {
        scan_prop_key(&m.key, flags);
        for d in &m.decorators {
            scan_expr(d, flags);
        }
        if let Some(v) = &m.value {
            scan_expr(v, flags);
        }
    }
}

fn scan_prop_key(k: &PropKey, flags: &mut u8) {
    match k {
        PropKey::Ident(_) | PropKey::Str(_) | PropKey::Num(_) => {}
        PropKey::Computed(e) => scan_expr(e, flags),
    }
}

fn scan_pattern(p: &Pattern, flags: &mut u8) {
    match p {
        Pattern::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Array(elems) => {
            for el in elems {
                match el {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, default } => {
                        scan_pattern(pattern, flags);
                        if let Some(d) = default {
                            scan_expr(d, flags);
                        }
                    }
                    ArrayPatElem::Rest(pat) => scan_pattern(pat, flags),
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &op.props {
                scan_prop_key(&prop.key, flags);
                scan_pattern(&prop.value, flags);
                if let Some(d) = &prop.default {
                    scan_expr(d, flags);
                }
            }
            if op.rest.as_deref() == Some("arguments") {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Member(e) => scan_expr(e, flags),
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}
