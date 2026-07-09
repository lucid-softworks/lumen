//! A compact binary (de)serializer for the parse AST, so the runtime's static JS glue can be
//! parsed once at build time and *decoded* (much cheaper than lex+parse) on every boot.
//!
//! Design points:
//! - **Optimization, not source of truth.** [`decode`] can fail (version skew, truncation); the
//!   caller falls back to re-parsing the original source, so a codec bug can never miscompile —
//!   at worst it costs a parse. A `MAGIC`+`VERSION` header makes skew a clean decode error.
//! - **Only parser output is encoded.** `Function`'s `scan`/`hoist`/`calls`/`code` are lazy
//!   runtime caches (`Cell`/`OnceCell`); decode initializes them empty, exactly as the parser
//!   leaves them, so a decoded tree is indistinguishable from a freshly parsed one.
//! - Interned `&'static str` operators are re-interned from [`KEYWORDS`]/[`PUNCTUATORS`] on
//!   decode (every op the parser emits comes from those tables).
//!
//! The format is deliberately dumb — a preorder walk with a `u8` tag per enum, LEB128 lengths,
//! little-endian `f64`. It is an internal build/runtime contract, never persisted across builds.

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use crate::ast::*;
use crate::bigint::JsBigInt;
use crate::token::{KEYWORDS, PUNCTUATORS};

const MAGIC: u32 = 0x4c_53_4e_31; // "LSN1"
/// Bump on any AST or format change. A mismatch makes `decode` fail → caller re-parses.
const VERSION: u32 = 1;

// ---- writer / reader --------------------------------------------------------------------------

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    /// LEB128 unsigned varint (small counts/tags stay one byte).
    fn uv(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                break;
            }
            self.buf.push(byte | 0x80);
        }
    }
    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    fn str(&mut self, s: &str) {
        self.uv(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, String>;

impl Reader<'_> {
    fn u8(&mut self) -> R<u8> {
        let b = *self.buf.get(self.pos).ok_or("snapshot: truncated")?;
        self.pos += 1;
        Ok(b)
    }
    fn uv(&mut self) -> R<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err("snapshot: varint overflow".into());
            }
        }
    }
    fn f64(&mut self) -> R<f64> {
        let end = self.pos + 8;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated f64")?;
        self.pos = end;
        Ok(f64::from_le_bytes(bytes.try_into().unwrap()))
    }
    fn bool(&mut self) -> R<bool> {
        Ok(self.u8()? != 0)
    }
    fn str(&mut self) -> R<String> {
        let len = self.uv()? as usize;
        let end = self.pos + len;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated str")?;
        self.pos = end;
        String::from_utf8(bytes.to_vec()).map_err(|_| "snapshot: bad utf8".into())
    }
    fn rcstr(&mut self) -> R<Rc<str>> {
        Ok(Rc::from(self.str()?.as_str()))
    }
}

/// Re-intern an operator string to the `&'static str` the parser would have used.
fn intern_op(s: &str) -> R<&'static str> {
    KEYWORDS
        .iter()
        .chain(PUNCTUATORS.iter())
        .find(|k| **k == s)
        .copied()
        .ok_or_else(|| format!("snapshot: unknown operator {s:?}"))
}

// ---- public API -------------------------------------------------------------------------------

/// Encode a parsed script body to a snapshot blob.
pub fn encode(body: &[Stmt]) -> Vec<u8> {
    let mut w = Writer {
        buf: Vec::with_capacity(body.len() * 32),
    };
    w.uv(MAGIC as u64);
    w.uv(VERSION as u64);
    enc_stmts(&mut w, body);
    w.buf
}

/// Decode a snapshot blob back into a script body. `Err` (skew/truncation/corruption) tells the
/// caller to fall back to parsing the original source.
pub fn decode(bytes: &[u8]) -> R<Vec<Stmt>> {
    let mut r = Reader { buf: bytes, pos: 0 };
    if r.uv()? != MAGIC as u64 {
        return Err("snapshot: bad magic".into());
    }
    if r.uv()? != VERSION as u64 {
        return Err("snapshot: version mismatch".into());
    }
    let body = dec_stmts(&mut r)?;
    Ok(body)
}

// ---- Vec / Option helpers (monomorphized by hand to keep the reader borrow simple) ------------

fn enc_stmts(w: &mut Writer, v: &[Stmt]) {
    w.uv(v.len() as u64);
    for s in v {
        enc_stmt(w, s);
    }
}
fn dec_stmts(r: &mut Reader) -> R<Vec<Stmt>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec_stmt(r)?);
    }
    Ok(out)
}

fn enc_exprs(w: &mut Writer, v: &[Expr]) {
    w.uv(v.len() as u64);
    for e in v {
        enc_expr(w, e);
    }
}
fn dec_exprs(r: &mut Reader) -> R<Vec<Expr>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec_expr(r)?);
    }
    Ok(out)
}

fn enc_opt_expr(w: &mut Writer, o: &Option<Expr>) {
    match o {
        Some(e) => {
            w.u8(1);
            enc_expr(w, e);
        }
        None => w.u8(0),
    }
}
fn dec_opt_expr(r: &mut Reader) -> R<Option<Expr>> {
    Ok(if r.u8()? == 1 {
        Some(dec_expr(r)?)
    } else {
        None
    })
}

fn enc_opt_str(w: &mut Writer, o: &Option<String>) {
    match o {
        Some(s) => {
            w.u8(1);
            w.str(s);
        }
        None => w.u8(0),
    }
}
fn dec_opt_str(r: &mut Reader) -> R<Option<String>> {
    Ok(if r.u8()? == 1 { Some(r.str()?) } else { None })
}

fn enc_opt_rcstr(w: &mut Writer, o: &Option<Rc<str>>) {
    match o {
        Some(s) => {
            w.u8(1);
            w.str(s);
        }
        None => w.u8(0),
    }
}
fn dec_opt_rcstr(r: &mut Reader) -> R<Option<Rc<str>>> {
    Ok(if r.u8()? == 1 { Some(r.rcstr()?) } else { None })
}

// ---- Stmt -------------------------------------------------------------------------------------

fn enc_stmt(w: &mut Writer, s: &Stmt) {
    match s {
        Stmt::Expr(e) => {
            w.u8(0);
            enc_expr(w, e);
        }
        Stmt::VarDecl { kind, decls } => {
            w.u8(1);
            enc_declkind(w, *kind);
            w.uv(decls.len() as u64);
            for (pat, init) in decls {
                enc_pattern(w, pat);
                enc_opt_expr(w, init);
            }
        }
        Stmt::FuncDecl(f) => {
            w.u8(2);
            enc_function(w, f);
        }
        Stmt::Return(e) => {
            w.u8(3);
            enc_opt_expr(w, e);
        }
        Stmt::If { test, cons, alt } => {
            w.u8(4);
            enc_expr(w, test);
            enc_stmt(w, cons);
            match alt {
                Some(a) => {
                    w.u8(1);
                    enc_stmt(w, a);
                }
                None => w.u8(0),
            }
        }
        Stmt::Block(b) => {
            w.u8(5);
            enc_stmts(w, b);
        }
        Stmt::While { test, body } => {
            w.u8(6);
            enc_expr(w, test);
            enc_stmt(w, body);
        }
        Stmt::DoWhile { body, test } => {
            w.u8(7);
            enc_stmt(w, body);
            enc_expr(w, test);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            w.u8(8);
            match init {
                Some(fi) => {
                    w.u8(1);
                    enc_forinit(w, fi);
                }
                None => w.u8(0),
            }
            enc_opt_expr(w, test);
            enc_opt_expr(w, update);
            enc_stmt(w, body);
        }
        Stmt::ForInOf {
            decl,
            left,
            right,
            of,
            is_await,
            body,
        } => {
            w.u8(9);
            match decl {
                Some(k) => {
                    w.u8(1);
                    enc_declkind(w, *k);
                }
                None => w.u8(0),
            }
            enc_pattern(w, left);
            enc_expr(w, right);
            w.bool(*of);
            w.bool(*is_await);
            enc_stmt(w, body);
        }
        Stmt::Break(l) => {
            w.u8(10);
            enc_opt_str(w, l);
        }
        Stmt::Continue(l) => {
            w.u8(11);
            enc_opt_str(w, l);
        }
        Stmt::Throw(e) => {
            w.u8(12);
            enc_expr(w, e);
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            w.u8(13);
            enc_stmts(w, block);
            match handler {
                Some((param, hbody)) => {
                    w.u8(1);
                    match param {
                        Some(p) => {
                            w.u8(1);
                            enc_pattern(w, p);
                        }
                        None => w.u8(0),
                    }
                    enc_stmts(w, hbody);
                }
                None => w.u8(0),
            }
            match finalizer {
                Some(f) => {
                    w.u8(1);
                    enc_stmts(w, f);
                }
                None => w.u8(0),
            }
        }
        Stmt::Switch { disc, cases } => {
            w.u8(14);
            enc_expr(w, disc);
            w.uv(cases.len() as u64);
            for c in cases {
                enc_opt_expr(w, &c.test);
                enc_stmts(w, &c.body);
            }
        }
        Stmt::Labeled { label, body } => {
            w.u8(15);
            w.str(label);
            enc_stmt(w, body);
        }
        Stmt::With { obj, body } => {
            w.u8(16);
            enc_expr(w, obj);
            enc_stmt(w, body);
        }
        Stmt::ClassDecl(c) => {
            w.u8(17);
            enc_class(w, c);
        }
        Stmt::Empty => w.u8(18),
        Stmt::Debugger => w.u8(19),
        Stmt::Import(d) => {
            w.u8(20);
            w.str(&d.source);
            w.uv(d.specs.len() as u64);
            for s in &d.specs {
                enc_importspec(w, s);
            }
            enc_opt_str(w, &d.attr_type);
        }
        Stmt::ExportNamed { specs, source } => {
            w.u8(21);
            w.uv(specs.len() as u64);
            for s in specs {
                w.str(&s.local);
                w.str(&s.exported);
            }
            enc_opt_rcstr(w, source);
        }
        Stmt::ExportDecl(s) => {
            w.u8(22);
            enc_stmt(w, s);
        }
        Stmt::ExportDefault(s) => {
            w.u8(23);
            enc_stmt(w, s);
        }
        Stmt::ExportAll { source, exported } => {
            w.u8(24);
            w.str(source);
            enc_opt_str(w, exported);
        }
    }
}

fn dec_stmt(r: &mut Reader) -> R<Stmt> {
    Ok(match r.u8()? {
        0 => Stmt::Expr(dec_expr(r)?),
        1 => {
            let kind = dec_declkind(r)?;
            let n = r.uv()? as usize;
            let mut decls = Vec::with_capacity(n);
            for _ in 0..n {
                decls.push((dec_pattern(r)?, dec_opt_expr(r)?));
            }
            Stmt::VarDecl { kind, decls }
        }
        2 => Stmt::FuncDecl(Rc::new(dec_function(r)?)),
        3 => Stmt::Return(dec_opt_expr(r)?),
        4 => {
            let test = dec_expr(r)?;
            let cons = Box::new(dec_stmt(r)?);
            let alt = if r.u8()? == 1 {
                Some(Box::new(dec_stmt(r)?))
            } else {
                None
            };
            Stmt::If { test, cons, alt }
        }
        5 => Stmt::Block(dec_stmts(r)?),
        6 => Stmt::While {
            test: dec_expr(r)?,
            body: Box::new(dec_stmt(r)?),
        },
        7 => Stmt::DoWhile {
            body: Box::new(dec_stmt(r)?),
            test: dec_expr(r)?,
        },
        8 => {
            let init = if r.u8()? == 1 {
                Some(Box::new(dec_forinit(r)?))
            } else {
                None
            };
            Stmt::For {
                init,
                test: dec_opt_expr(r)?,
                update: dec_opt_expr(r)?,
                body: Box::new(dec_stmt(r)?),
            }
        }
        9 => {
            let decl = if r.u8()? == 1 {
                Some(dec_declkind(r)?)
            } else {
                None
            };
            Stmt::ForInOf {
                decl,
                left: dec_pattern(r)?,
                right: dec_expr(r)?,
                of: r.bool()?,
                is_await: r.bool()?,
                body: Box::new(dec_stmt(r)?),
            }
        }
        10 => Stmt::Break(dec_opt_str(r)?),
        11 => Stmt::Continue(dec_opt_str(r)?),
        12 => Stmt::Throw(dec_expr(r)?),
        13 => {
            let block = dec_stmts(r)?;
            let handler = if r.u8()? == 1 {
                let param = if r.u8()? == 1 {
                    Some(dec_pattern(r)?)
                } else {
                    None
                };
                Some((param, dec_stmts(r)?))
            } else {
                None
            };
            let finalizer = if r.u8()? == 1 {
                Some(dec_stmts(r)?)
            } else {
                None
            };
            Stmt::Try {
                block,
                handler,
                finalizer,
            }
        }
        14 => {
            let disc = dec_expr(r)?;
            let n = r.uv()? as usize;
            let mut cases = Vec::with_capacity(n);
            for _ in 0..n {
                cases.push(SwitchCase {
                    test: dec_opt_expr(r)?,
                    body: dec_stmts(r)?,
                });
            }
            Stmt::Switch { disc, cases }
        }
        15 => Stmt::Labeled {
            label: r.str()?,
            body: Box::new(dec_stmt(r)?),
        },
        16 => Stmt::With {
            obj: dec_expr(r)?,
            body: Box::new(dec_stmt(r)?),
        },
        17 => Stmt::ClassDecl(Rc::new(dec_class(r)?)),
        18 => Stmt::Empty,
        19 => Stmt::Debugger,
        20 => {
            let source = r.rcstr()?;
            let n = r.uv()? as usize;
            let mut specs = Vec::with_capacity(n);
            for _ in 0..n {
                specs.push(dec_importspec(r)?);
            }
            Stmt::Import(ImportDecl {
                source,
                specs,
                attr_type: dec_opt_str(r)?,
            })
        }
        21 => {
            let n = r.uv()? as usize;
            let mut specs = Vec::with_capacity(n);
            for _ in 0..n {
                specs.push(ExportSpec {
                    local: r.str()?,
                    exported: r.str()?,
                });
            }
            Stmt::ExportNamed {
                specs,
                source: dec_opt_rcstr(r)?,
            }
        }
        22 => Stmt::ExportDecl(Box::new(dec_stmt(r)?)),
        23 => Stmt::ExportDefault(Box::new(dec_stmt(r)?)),
        24 => Stmt::ExportAll {
            source: r.rcstr()?,
            exported: dec_opt_str(r)?,
        },
        t => return Err(format!("snapshot: bad Stmt tag {t}")),
    })
}

// ---- Expr -------------------------------------------------------------------------------------

fn enc_expr(w: &mut Writer, e: &Expr) {
    match e {
        Expr::Paren(x) => {
            w.u8(0);
            enc_expr(w, x);
        }
        Expr::Num(n) => {
            w.u8(1);
            w.f64(*n);
        }
        Expr::BigInt(b) => {
            w.u8(2);
            w.str(&b.to_string_radix(16));
        }
        Expr::Str(s) => {
            w.u8(3);
            w.str(s);
        }
        Expr::ToStr(x) => {
            w.u8(4);
            enc_expr(w, x);
        }
        Expr::Bool(b) => {
            w.u8(5);
            w.bool(*b);
        }
        Expr::Null => w.u8(6),
        Expr::Undefined => w.u8(7),
        Expr::Ident(n) => {
            w.u8(8);
            w.str(n);
        }
        Expr::This => w.u8(9),
        Expr::Regex { body, flags } => {
            w.u8(10);
            w.str(body);
            w.str(flags);
        }
        Expr::Array(elems) => {
            w.u8(11);
            enc_array_elems(w, elems);
        }
        Expr::Object(props) => {
            w.u8(12);
            w.uv(props.len() as u64);
            for p in props {
                enc_propdef(w, p);
            }
        }
        Expr::Func(f) => {
            w.u8(13);
            enc_function(w, f);
        }
        Expr::Class(c) => {
            w.u8(14);
            enc_class(w, c);
        }
        Expr::Yield { delegate, arg } => {
            w.u8(15);
            w.bool(*delegate);
            match arg {
                Some(a) => {
                    w.u8(1);
                    enc_expr(w, a);
                }
                None => w.u8(0),
            }
        }
        Expr::Await(x) => {
            w.u8(16);
            enc_expr(w, x);
        }
        Expr::Super => w.u8(17),
        Expr::Unary { op, arg } => {
            w.u8(18);
            w.str(op);
            enc_expr(w, arg);
        }
        Expr::Update { op, prefix, arg } => {
            w.u8(19);
            w.str(op);
            w.bool(*prefix);
            enc_expr(w, arg);
        }
        Expr::Binary { op, left, right } => {
            w.u8(20);
            w.str(op);
            enc_expr(w, left);
            enc_expr(w, right);
        }
        Expr::Logical { op, left, right } => {
            w.u8(21);
            w.str(op);
            enc_expr(w, left);
            enc_expr(w, right);
        }
        Expr::Assign { op, target, value } => {
            w.u8(22);
            w.str(op);
            enc_expr(w, target);
            enc_expr(w, value);
        }
        Expr::Cond { test, cons, alt } => {
            w.u8(23);
            enc_expr(w, test);
            enc_expr(w, cons);
            enc_expr(w, alt);
        }
        Expr::Call {
            callee,
            args,
            optional,
        } => {
            w.u8(24);
            enc_expr(w, callee);
            enc_array_elems(w, args);
            w.bool(*optional);
        }
        Expr::New { callee, args } => {
            w.u8(25);
            enc_expr(w, callee);
            enc_array_elems(w, args);
        }
        Expr::Member {
            obj,
            prop,
            optional,
        } => {
            w.u8(26);
            enc_expr(w, obj);
            w.str(prop);
            w.bool(*optional);
        }
        Expr::Index {
            obj,
            index,
            optional,
        } => {
            w.u8(27);
            enc_expr(w, obj);
            enc_expr(w, index);
            w.bool(*optional);
        }
        Expr::Seq(exprs) => {
            w.u8(28);
            enc_exprs(w, exprs);
        }
        Expr::TaggedTemplate { tag, quasis, subs } => {
            w.u8(29);
            enc_expr(w, tag);
            w.uv(quasis.len() as u64);
            for (cooked, raw) in quasis {
                enc_opt_str(w, cooked);
                w.str(raw);
            }
            enc_exprs(w, subs);
        }
        Expr::OptionalChain(x) => {
            w.u8(30);
            enc_expr(w, x);
        }
        Expr::PrivateIn { name, obj } => {
            w.u8(31);
            w.str(name);
            enc_expr(w, obj);
        }
        Expr::ImportCall {
            spec,
            phase,
            options,
        } => {
            w.u8(32);
            enc_expr(w, spec);
            w.u8(match phase {
                ImportPhase::Evaluation => 0,
                ImportPhase::Source => 1,
                ImportPhase::Defer => 2,
            });
            match options {
                Some(o) => {
                    w.u8(1);
                    enc_expr(w, o);
                }
                None => w.u8(0),
            }
        }
        Expr::ImportMeta => w.u8(33),
        Expr::NewTarget => w.u8(34),
    }
}

fn dec_expr(r: &mut Reader) -> R<Expr> {
    Ok(match r.u8()? {
        0 => Expr::Paren(Box::new(dec_expr(r)?)),
        1 => Expr::Num(r.f64()?),
        2 => Expr::BigInt(JsBigInt::parse_radix(&r.str()?, 16).ok_or("snapshot: bad bigint")?),
        3 => Expr::Str(r.rcstr()?),
        4 => Expr::ToStr(Box::new(dec_expr(r)?)),
        5 => Expr::Bool(r.bool()?),
        6 => Expr::Null,
        7 => Expr::Undefined,
        8 => Expr::Ident(r.str()?),
        9 => Expr::This,
        10 => Expr::Regex {
            body: r.rcstr()?,
            flags: r.rcstr()?,
        },
        11 => Expr::Array(dec_array_elems(r)?),
        12 => {
            let n = r.uv()? as usize;
            let mut props = Vec::with_capacity(n);
            for _ in 0..n {
                props.push(dec_propdef(r)?);
            }
            Expr::Object(props)
        }
        13 => Expr::Func(Rc::new(dec_function(r)?)),
        14 => Expr::Class(Rc::new(dec_class(r)?)),
        15 => {
            let delegate = r.bool()?;
            let arg = if r.u8()? == 1 {
                Some(Box::new(dec_expr(r)?))
            } else {
                None
            };
            Expr::Yield { delegate, arg }
        }
        16 => Expr::Await(Box::new(dec_expr(r)?)),
        17 => Expr::Super,
        18 => Expr::Unary {
            op: intern_op(&r.str()?)?,
            arg: Box::new(dec_expr(r)?),
        },
        19 => Expr::Update {
            op: intern_op(&r.str()?)?,
            prefix: r.bool()?,
            arg: Box::new(dec_expr(r)?),
        },
        20 => Expr::Binary {
            op: intern_op(&r.str()?)?,
            left: Box::new(dec_expr(r)?),
            right: Box::new(dec_expr(r)?),
        },
        21 => Expr::Logical {
            op: intern_op(&r.str()?)?,
            left: Box::new(dec_expr(r)?),
            right: Box::new(dec_expr(r)?),
        },
        22 => Expr::Assign {
            op: intern_op(&r.str()?)?,
            target: Box::new(dec_expr(r)?),
            value: Box::new(dec_expr(r)?),
        },
        23 => Expr::Cond {
            test: Box::new(dec_expr(r)?),
            cons: Box::new(dec_expr(r)?),
            alt: Box::new(dec_expr(r)?),
        },
        24 => Expr::Call {
            callee: Box::new(dec_expr(r)?),
            args: dec_array_elems(r)?,
            optional: r.bool()?,
        },
        25 => Expr::New {
            callee: Box::new(dec_expr(r)?),
            args: dec_array_elems(r)?,
        },
        26 => Expr::Member {
            obj: Box::new(dec_expr(r)?),
            prop: r.str()?,
            optional: r.bool()?,
        },
        27 => Expr::Index {
            obj: Box::new(dec_expr(r)?),
            index: Box::new(dec_expr(r)?),
            optional: r.bool()?,
        },
        28 => Expr::Seq(dec_exprs(r)?),
        29 => {
            let tag = Box::new(dec_expr(r)?);
            let n = r.uv()? as usize;
            let mut quasis = Vec::with_capacity(n);
            for _ in 0..n {
                quasis.push((dec_opt_str(r)?, r.str()?));
            }
            Expr::TaggedTemplate {
                tag,
                quasis,
                subs: dec_exprs(r)?,
            }
        }
        30 => Expr::OptionalChain(Box::new(dec_expr(r)?)),
        31 => Expr::PrivateIn {
            name: r.str()?,
            obj: Box::new(dec_expr(r)?),
        },
        32 => Expr::ImportCall {
            spec: Box::new(dec_expr(r)?),
            phase: match r.u8()? {
                0 => ImportPhase::Evaluation,
                1 => ImportPhase::Source,
                2 => ImportPhase::Defer,
                t => return Err(format!("snapshot: bad ImportPhase {t}")),
            },
            options: if r.u8()? == 1 {
                Some(Box::new(dec_expr(r)?))
            } else {
                None
            },
        },
        33 => Expr::ImportMeta,
        34 => Expr::NewTarget,
        t => return Err(format!("snapshot: bad Expr tag {t}")),
    })
}

// ---- shared sub-structures --------------------------------------------------------------------

fn enc_array_elems(w: &mut Writer, elems: &[ArrayElem]) {
    w.uv(elems.len() as u64);
    for el in elems {
        match el {
            ArrayElem::Item(e) => {
                w.u8(0);
                enc_expr(w, e);
            }
            ArrayElem::Spread(e) => {
                w.u8(1);
                enc_expr(w, e);
            }
            ArrayElem::Hole => w.u8(2),
        }
    }
}
fn dec_array_elems(r: &mut Reader) -> R<Vec<ArrayElem>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(match r.u8()? {
            0 => ArrayElem::Item(dec_expr(r)?),
            1 => ArrayElem::Spread(dec_expr(r)?),
            2 => ArrayElem::Hole,
            t => return Err(format!("snapshot: bad ArrayElem {t}")),
        });
    }
    Ok(out)
}

fn enc_propdef(w: &mut Writer, p: &PropDef) {
    match p {
        PropDef::KeyValue { key, value } => {
            w.u8(0);
            enc_propkey(w, key);
            enc_expr(w, value);
        }
        PropDef::Cover { key, value } => {
            w.u8(1);
            enc_propkey(w, key);
            enc_expr(w, value);
        }
        PropDef::Method { key, func } => {
            w.u8(2);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Getter { key, func } => {
            w.u8(3);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Setter { key, func } => {
            w.u8(4);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Spread(e) => {
            w.u8(5);
            enc_expr(w, e);
        }
        PropDef::Proto(e) => {
            w.u8(6);
            enc_expr(w, e);
        }
    }
}
fn dec_propdef(r: &mut Reader) -> R<PropDef> {
    Ok(match r.u8()? {
        0 => PropDef::KeyValue {
            key: dec_propkey(r)?,
            value: dec_expr(r)?,
        },
        1 => PropDef::Cover {
            key: dec_propkey(r)?,
            value: dec_expr(r)?,
        },
        2 => PropDef::Method {
            key: dec_propkey(r)?,
            func: Rc::new(dec_function(r)?),
        },
        3 => PropDef::Getter {
            key: dec_propkey(r)?,
            func: Rc::new(dec_function(r)?),
        },
        4 => PropDef::Setter {
            key: dec_propkey(r)?,
            func: Rc::new(dec_function(r)?),
        },
        5 => PropDef::Spread(dec_expr(r)?),
        6 => PropDef::Proto(dec_expr(r)?),
        t => return Err(format!("snapshot: bad PropDef {t}")),
    })
}

fn enc_propkey(w: &mut Writer, k: &PropKey) {
    match k {
        PropKey::Ident(n) => {
            w.u8(0);
            w.str(n);
        }
        PropKey::Str(s) => {
            w.u8(1);
            w.str(s);
        }
        PropKey::Num(n) => {
            w.u8(2);
            w.f64(*n);
        }
        PropKey::Computed(e) => {
            w.u8(3);
            enc_expr(w, e);
        }
    }
}
fn dec_propkey(r: &mut Reader) -> R<PropKey> {
    Ok(match r.u8()? {
        0 => PropKey::Ident(r.str()?),
        1 => PropKey::Str(r.rcstr()?),
        2 => PropKey::Num(r.f64()?),
        3 => PropKey::Computed(dec_expr(r)?),
        t => return Err(format!("snapshot: bad PropKey {t}")),
    })
}

fn enc_pattern(w: &mut Writer, p: &Pattern) {
    match p {
        Pattern::Ident(n) => {
            w.u8(0);
            w.str(n);
        }
        Pattern::Array(elems) => {
            w.u8(1);
            w.uv(elems.len() as u64);
            for el in elems {
                match el {
                    ArrayPatElem::Hole => w.u8(0),
                    ArrayPatElem::Elem { pattern, default } => {
                        w.u8(1);
                        enc_pattern(w, pattern);
                        enc_opt_expr(w, default);
                    }
                    ArrayPatElem::Rest(p) => {
                        w.u8(2);
                        enc_pattern(w, p);
                    }
                }
            }
        }
        Pattern::Object(op) => {
            w.u8(2);
            w.uv(op.props.len() as u64);
            for prop in &op.props {
                enc_propkey(w, &prop.key);
                enc_pattern(w, &prop.value);
                enc_opt_expr(w, &prop.default);
            }
            enc_opt_str(w, &op.rest);
        }
        Pattern::Member(e) => {
            w.u8(3);
            enc_expr(w, e);
        }
    }
}
fn dec_pattern(r: &mut Reader) -> R<Pattern> {
    Ok(match r.u8()? {
        0 => Pattern::Ident(r.str()?),
        1 => {
            let n = r.uv()? as usize;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(match r.u8()? {
                    0 => ArrayPatElem::Hole,
                    1 => ArrayPatElem::Elem {
                        pattern: dec_pattern(r)?,
                        default: dec_opt_expr(r)?,
                    },
                    2 => ArrayPatElem::Rest(dec_pattern(r)?),
                    t => return Err(format!("snapshot: bad ArrayPatElem {t}")),
                });
            }
            Pattern::Array(elems)
        }
        2 => {
            let n = r.uv()? as usize;
            let mut props = Vec::with_capacity(n);
            for _ in 0..n {
                props.push(ObjPatProp {
                    key: dec_propkey(r)?,
                    value: dec_pattern(r)?,
                    default: dec_opt_expr(r)?,
                });
            }
            Pattern::Object(ObjectPat {
                props,
                rest: dec_opt_str(r)?,
            })
        }
        3 => Pattern::Member(Box::new(dec_expr(r)?)),
        t => return Err(format!("snapshot: bad Pattern {t}")),
    })
}

fn enc_function(w: &mut Writer, f: &Function) {
    enc_opt_str(w, &f.name);
    w.uv(f.params.len() as u64);
    for p in &f.params {
        enc_pattern(w, &p.pattern);
        enc_opt_expr(w, &p.default);
        w.bool(p.rest);
    }
    enc_stmts(w, &f.body);
    // Flags packed into one byte.
    let flags = (f.is_arrow as u8)
        | (f.is_strict as u8) << 1
        | (f.expr_body as u8) << 2
        | (f.is_generator as u8) << 3
        | (f.is_async as u8) << 4
        | (f.is_method as u8) << 5
        | (f.is_fn_expr as u8) << 6;
    w.u8(flags);
    enc_opt_rcstr(w, &f.source);
}
fn dec_function(r: &mut Reader) -> R<Function> {
    let name = dec_opt_str(r)?;
    let n = r.uv()? as usize;
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        params.push(Param {
            pattern: dec_pattern(r)?,
            default: dec_opt_expr(r)?,
            rest: r.bool()?,
        });
    }
    let body = dec_stmts(r)?;
    let flags = r.u8()?;
    let source = dec_opt_rcstr(r)?;
    Ok(Function {
        name,
        params,
        body,
        is_arrow: flags & 1 != 0,
        is_strict: flags & 2 != 0,
        expr_body: flags & 4 != 0,
        is_generator: flags & 8 != 0,
        is_async: flags & 16 != 0,
        is_method: flags & 32 != 0,
        is_fn_expr: flags & 64 != 0,
        source,
        // Lazy runtime caches — start empty, exactly as the parser leaves them.
        scan: Cell::new(0),
        hoist: OnceCell::new(),
        calls: Cell::new(0),
        code: OnceCell::new(),
        code2: OnceCell::new(),
    })
}

fn enc_class(w: &mut Writer, c: &Class) {
    enc_opt_str(w, &c.name);
    match &c.superclass {
        Some(sc) => {
            w.u8(1);
            enc_expr(w, sc);
        }
        None => w.u8(0),
    }
    w.uv(c.members.len() as u64);
    for m in &c.members {
        enc_propkey(w, &m.key);
        w.u8(match m.kind {
            MemberKind::Constructor => 0,
            MemberKind::Method => 1,
            MemberKind::Get => 2,
            MemberKind::Set => 3,
            MemberKind::Field => 4,
            MemberKind::Accessor => 5,
            MemberKind::StaticBlock => 6,
        });
        w.bool(m.is_static);
        match &m.func {
            Some(f) => {
                w.u8(1);
                enc_function(w, f);
            }
            None => w.u8(0),
        }
        enc_opt_expr(w, &m.value);
        enc_exprs(w, &m.decorators);
    }
    enc_exprs(w, &c.decorators);
    enc_opt_rcstr(w, &c.source);
}
fn dec_class(r: &mut Reader) -> R<Class> {
    let name = dec_opt_str(r)?;
    let superclass = if r.u8()? == 1 {
        Some(Box::new(dec_expr(r)?))
    } else {
        None
    };
    let n = r.uv()? as usize;
    let mut members = Vec::with_capacity(n);
    for _ in 0..n {
        let key = dec_propkey(r)?;
        let kind = match r.u8()? {
            0 => MemberKind::Constructor,
            1 => MemberKind::Method,
            2 => MemberKind::Get,
            3 => MemberKind::Set,
            4 => MemberKind::Field,
            5 => MemberKind::Accessor,
            6 => MemberKind::StaticBlock,
            t => return Err(format!("snapshot: bad MemberKind {t}")),
        };
        let is_static = r.bool()?;
        let func = if r.u8()? == 1 {
            Some(Rc::new(dec_function(r)?))
        } else {
            None
        };
        members.push(ClassMember {
            key,
            kind,
            is_static,
            func,
            value: dec_opt_expr(r)?,
            decorators: dec_exprs(r)?,
        });
    }
    Ok(Class {
        name,
        superclass,
        members,
        decorators: dec_exprs(r)?,
        source: dec_opt_rcstr(r)?,
    })
}

fn enc_forinit(w: &mut Writer, fi: &ForInit) {
    match fi {
        ForInit::VarDecl { kind, decls } => {
            w.u8(0);
            enc_declkind(w, *kind);
            w.uv(decls.len() as u64);
            for (pat, init) in decls {
                enc_pattern(w, pat);
                enc_opt_expr(w, init);
            }
        }
        ForInit::Expr(e) => {
            w.u8(1);
            enc_expr(w, e);
        }
    }
}
fn dec_forinit(r: &mut Reader) -> R<ForInit> {
    Ok(match r.u8()? {
        0 => {
            let kind = dec_declkind(r)?;
            let n = r.uv()? as usize;
            let mut decls = Vec::with_capacity(n);
            for _ in 0..n {
                decls.push((dec_pattern(r)?, dec_opt_expr(r)?));
            }
            ForInit::VarDecl { kind, decls }
        }
        1 => ForInit::Expr(dec_expr(r)?),
        t => return Err(format!("snapshot: bad ForInit {t}")),
    })
}

fn enc_importspec(w: &mut Writer, s: &ImportSpec) {
    match s {
        ImportSpec::Default(n) => {
            w.u8(0);
            w.str(n);
        }
        ImportSpec::Namespace(n) => {
            w.u8(1);
            w.str(n);
        }
        ImportSpec::DeferNamespace(n) => {
            w.u8(2);
            w.str(n);
        }
        ImportSpec::Source(n) => {
            w.u8(3);
            w.str(n);
        }
        ImportSpec::Named { imported, local } => {
            w.u8(4);
            w.str(imported);
            w.str(local);
        }
    }
}
fn dec_importspec(r: &mut Reader) -> R<ImportSpec> {
    Ok(match r.u8()? {
        0 => ImportSpec::Default(r.str()?),
        1 => ImportSpec::Namespace(r.str()?),
        2 => ImportSpec::DeferNamespace(r.str()?),
        3 => ImportSpec::Source(r.str()?),
        4 => ImportSpec::Named {
            imported: r.str()?,
            local: r.str()?,
        },
        t => return Err(format!("snapshot: bad ImportSpec {t}")),
    })
}

fn enc_declkind(w: &mut Writer, k: DeclKind) {
    w.u8(match k {
        DeclKind::Var => 0,
        DeclKind::Let => 1,
        DeclKind::Const => 2,
        DeclKind::Using => 3,
        DeclKind::AwaitUsing => 4,
    });
}
fn dec_declkind(r: &mut Reader) -> R<DeclKind> {
    Ok(match r.u8()? {
        0 => DeclKind::Var,
        1 => DeclKind::Let,
        2 => DeclKind::Const,
        3 => DeclKind::Using,
        4 => DeclKind::AwaitUsing,
        t => return Err(format!("snapshot: bad DeclKind {t}")),
    })
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    /// `encode` and `decode` must be exact inverses: re-encoding a decoded tree reproduces the
    /// bytes. This catches every tag/field-order asymmetry (a *dropped* field is caught instead
    /// by the behavioral suites that run the decoded glue).
    fn assert_roundtrips(src: &str) {
        let blob = crate::compile_snapshot(src).unwrap_or_else(|e| panic!("compile {src:?}: {e}"));
        let ast = decode(&blob).unwrap_or_else(|e| panic!("decode {src:?}: {e}"));
        assert_eq!(blob, encode(&ast), "re-encode differs for: {src}");
    }

    #[test]
    fn roundtrip_diverse_constructs() {
        for src in [
            "1; 'two'; true; null; undefined; 0xffn; 1.5e10;",
            "let { a, b: [c, ...d] = [], ...rest } = obj; const [x = 1, , z] = arr;",
            "class C extends B { #f = 1; static s = 2; get g(){return this.#f} set g(v){} accessor a; static { init(); } has(o){ return #f in o } ['x'+y]() {} }",
            "async function* gen(a, b = 1, ...c) { yield* a; await b; for await (const x of c) {} }",
            "const f = (x) => x ? a?.b.c?.() : `t${x}${'raw'}`; tag`a${1}b`;",
            "try { throw new E(); } catch { } finally { } with (o) { o.p = 1; } label: for (k in o) break label;",
            "a ??= b; c ||= d; e &&= f; g **= h; -x; !y; typeof z; void 0; delete o.p; a in b; a instanceof B;",
            "switch (x) { case 1: break; default: } do { i++ } while (i < 10); function ctor(){ return new.target; }",
            "const p = import('m'); const q = import('m', { with: { type: 'json' } });",
            "obj = { __proto__: p, shorthand, key: v, [comp]: w, m() {}, get g() {}, *gen() {}, async am() {}, ...spread };",
        ] {
            assert_roundtrips(src);
        }
    }

    #[test]
    fn roundtrip_real_web_glue() {
        // The actual runtime glue — the thing the build-time snapshot will encode.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../lumen-web/src/js/");
        for file in [
            "events.js",
            "encoding.js",
            "url.js",
            "streams.js",
            "fetch.js",
            "server.js",
            "crypto.js",
        ] {
            let src = std::fs::read_to_string(format!("{dir}{file}")).unwrap();
            assert_roundtrips(&src);
        }
    }
}
