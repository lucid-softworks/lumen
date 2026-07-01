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
}
#[derive(Debug, Clone)]
pub enum ImportSpec {
    /// `import x from "…"`
    Default(String),
    /// `import * as ns from "…"`
    Namespace(String),
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
    Num(f64),
    BigInt(i128),
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
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}
