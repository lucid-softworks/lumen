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
//! - Function bodies are validated on load but materialized only when execution or analysis
//!   needs them. Nested body boundaries share an immutable byte buffer, avoiding repeated scans.
//!   Immediately called function expressions decode eagerly to avoid an extra validation pass.
//! - Interned `&'static str` operators are re-interned from the lexer's keyword/punctuation tables on
//!   decode (every op the parser emits comes from those tables).
//!
//! The format is a preorder walk with a `u8` tag per enum, LEB128 lengths, little-endian `f64`,
//! and a trailing table of shared source texts. Function/class source records reference UTF-8
//! ranges in that table. It is an internal build/runtime contract, never persisted across builds.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use crate::ast::*;
use crate::bigint::JsBigInt;
use crate::token::{keyword_ref, punctuation_ref};

mod validate;

struct DecodedSource {
    bytes: Rc<[u8]>,
    body_end: usize,
    roots: Vec<SharedSource>,
    body_ends: RefCell<crate::fasthash::FastMap<usize, usize>>,
}

/// A body range is exposed only after every tag, field and nested body has been validated.
#[derive(Clone)]
pub(crate) struct DeferredBody {
    source: Rc<DecodedSource>,
    start: usize,
    end: usize,
    depth: usize,
}

impl DeferredBody {
    pub(crate) fn decode(&self) -> Vec<Stmt> {
        let mut reader = Reader {
            buf: &self.source.bytes[..self.source.body_end],
            pos: self.start,
            depth: self.depth,
            source: Some(&self.source),
            roots: &self.source.roots,
        };
        let body = dec_stmts(&mut reader).expect("validated snapshot body must decode");
        assert_eq!(reader.pos, self.end, "validated snapshot body boundary");
        body
    }
}

const MAGIC: u32 = 0x4c_53_4e_31; // "LSN1"
/// Bump on any AST or format change. A mismatch makes `decode` fail → caller re-parses.
const VERSION: u32 = 2;

// ---- writer / reader --------------------------------------------------------------------------

struct Writer {
    buf: Vec<u8>,
    limit: Option<usize>,
    depth: usize,
    failed: bool,
    source_ids: crate::fasthash::FastMap<(u8, usize), usize>,
    source_roots: Vec<FunctionSource>,
}

impl Writer {
    fn new(limit: Option<usize>) -> Self {
        Self {
            buf: Vec::new(),
            limit,
            depth: 0,
            failed: false,
            source_ids: Default::default(),
            source_roots: Vec::new(),
        }
    }
    fn bytes(&mut self, bytes: &[u8]) {
        if self.failed {
            return;
        }
        if self
            .limit
            .is_some_and(|limit| bytes.len() > limit.saturating_sub(self.buf.len()))
        {
            self.failed = true;
            return;
        }
        self.buf.extend_from_slice(bytes);
    }
    fn enter(&mut self) -> bool {
        if self.failed || (self.limit.is_some() && self.depth >= 128) {
            self.failed = true;
            return false;
        }
        self.depth += 1;
        true
    }
    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    /// LEB128 unsigned varint (small counts/tags stay one byte).
    fn uv(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.u8(byte);
                break;
            }
            self.u8(byte | 0x80);
        }
    }
    fn f64(&mut self, v: f64) {
        self.bytes(&v.to_le_bytes());
    }
    fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    fn str(&mut self, s: &str) {
        self.uv(s.len() as u64);
        self.bytes(s.as_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
    source: Option<&'a Rc<DecodedSource>>,
    roots: &'a [SharedSource],
}

type R<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn record_body(&self, start: usize) {
        if let Some(source) = self.source {
            source.body_ends.borrow_mut().insert(start, self.pos);
        }
    }
    fn count(&mut self) -> R<usize> {
        let n = usize::try_from(self.uv()?).map_err(|_| "snapshot: length overflow")?;
        if n > self.buf.len() - self.pos {
            return Err("snapshot: truncated sequence".into());
        }
        Ok(n)
    }
    fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        if self.depth >= 128 {
            return Err("snapshot: nesting limit".into());
        }
        self.depth += 1;
        let result = read(self);
        self.depth -= 1;
        result
    }
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
            if shift == 63 && byte > 1 {
                return Err("snapshot: varint overflow".into());
            }
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
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("snapshot: bad boolean".into()),
        }
    }
    fn str_slice(&mut self) -> R<&'a str> {
        let len = self.count()?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or("snapshot: length overflow")?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated str")?;
        self.pos = end;
        std::str::from_utf8(bytes).map_err(|_| "snapshot: bad utf8".into())
    }
    fn str(&mut self) -> R<String> {
        Ok(self.str_slice()?.to_owned())
    }
    fn rcstr(&mut self) -> R<Rc<str>> {
        Ok(Rc::from(self.str_slice()?))
    }
}

/// Re-intern an operator string to the `&'static str` the parser would have used.
fn intern_op(s: &str) -> R<Operator> {
    let found = if s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        keyword_ref(s)
    } else {
        // Require a complete canonical spelling; a valid prefix followed by garbage
        // must still invalidate the snapshot.
        punctuation_ref(s)
    };
    found.ok_or_else(|| format!("snapshot: unknown operator {s:?}"))
}

// ---- public API -------------------------------------------------------------------------------

/// Encode a parsed script body to a snapshot blob.
pub fn encode(body: &[Stmt]) -> Vec<u8> {
    encode_with_limit(body, None).expect("unbounded snapshot writer")
}

/// Encode a disk-cache entry with bounded size and nesting; unsupported entries are skipped.
pub(crate) fn encode_limited(body: &[Stmt], limit: usize) -> Option<Vec<u8>> {
    encode_with_limit(body, Some(limit))
}

fn encode_with_limit(body: &[Stmt], limit: Option<usize>) -> Option<Vec<u8>> {
    let mut w = Writer::new(limit);
    w.uv(MAGIC as u64);
    w.uv(VERSION as u64);
    let offset_position = w.buf.len();
    w.bytes(&0u64.to_le_bytes());
    enc_stmts(&mut w, body);
    if w.failed {
        return None;
    }
    // Roots were discovered while encoding the AST, so no extra AST traversal is needed.
    let table_start = w.buf.len();
    w.uv(w.source_roots.len() as u64);
    for root in std::mem::take(&mut w.source_roots) {
        w.str(&root);
    }
    if !w.failed {
        w.buf[offset_position..offset_position + 8]
            .copy_from_slice(&(table_start as u64).to_le_bytes());
    }
    (!w.failed).then_some(w.buf)
}

/// Load every root before decoding any AST node. Deferred bodies then see an immutable table
/// regardless of when they are materialized; the AST reader cannot run into the table bytes.
fn decode_header(bytes: &[u8], backing: Option<&Rc<[u8]>>) -> R<(usize, usize, Vec<SharedSource>)> {
    let mut r = Reader {
        buf: bytes,
        pos: 0,
        depth: 0,
        source: None,
        roots: &[],
    };
    if r.uv()? != MAGIC as u64 {
        return Err("snapshot: bad magic".into());
    }
    if r.uv()? != VERSION as u64 {
        return Err("snapshot: version mismatch".into());
    }
    let offset_end = r.pos.checked_add(8).ok_or("snapshot: offset overflow")?;
    let offset = bytes
        .get(r.pos..offset_end)
        .ok_or("snapshot: missing source table offset")?;
    let body_end = usize::try_from(u64::from_le_bytes(offset.try_into().unwrap()))
        .map_err(|_| "snapshot: source table offset overflow")?;
    let body_start = offset_end;
    if body_end < body_start || body_end >= bytes.len() {
        return Err("snapshot: invalid source table offset".into());
    }
    r.pos = body_end;
    let count = r.count()?;
    // Every emitted root has a reference with at least four bytes in the AST. Also avoid
    // preallocating from an untrusted count before validating the corresponding strings.
    if count > (body_end - body_start) / 4 {
        return Err("snapshot: excessive source roots".into());
    }
    let mut roots = Vec::new();
    for _ in 0..count {
        let text = r.str_slice()?;
        roots.push(match backing {
            Some(bytes) => SharedSource::snapshot(bytes.clone(), r.pos - text.len(), text),
            None => SharedSource::text(text.to_owned()),
        });
    }
    if r.pos != bytes.len() {
        return Err("snapshot: trailing data".into());
    }
    Ok((body_start, body_end, roots))
}

/// Decode a snapshot blob back into a script body. `Err` (skew/truncation/corruption) tells the
/// caller to fall back to parsing the original source.
pub fn decode(bytes: &[u8]) -> R<Vec<Stmt>> {
    let bytes: Rc<[u8]> = Rc::from(bytes);
    let (body_start, body_end, roots) = decode_header(&bytes, Some(&bytes))?;
    let source = Rc::new(DecodedSource {
        bytes,
        body_end,
        roots,
        body_ends: RefCell::new(Default::default()),
    });
    decode_reader(Reader {
        buf: &source.bytes[..body_end],
        pos: body_start,
        depth: 0,
        source: Some(&source),
        roots: &source.roots,
    })
}

/// Boot snapshots execute immediately; decode directly without an encoded backing copy or
/// a separate validation pass. Disk-cache loads use `decode` to defer unused dependency bodies.
pub(crate) fn decode_eager(bytes: &[u8]) -> R<Vec<Stmt>> {
    let (body_start, body_end, roots) = decode_header(bytes, None)?;
    decode_reader(Reader {
        buf: &bytes[..body_end],
        pos: body_start,
        depth: 0,
        source: None,
        roots: &roots,
    })
}

fn decode_reader(mut r: Reader) -> R<Vec<Stmt>> {
    let body = dec_stmts(&mut r)?;
    if r.pos != r.buf.len() {
        return Err("snapshot: trailing data".into());
    }
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
    let n = r.count()?;
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
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec_expr(r)?);
    }
    Ok(out)
}

fn enc_opt_expr(w: &mut Writer, o: &Option<Expr>) {
    enc_opt_expr_ref(w, o.as_ref());
}

fn enc_opt_expr_ref(w: &mut Writer, o: Option<&Expr>) {
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

fn enc_opt_source(w: &mut Writer, source: &Option<FunctionSource>) {
    if w.failed {
        return;
    }
    if let Some(source) = source {
        w.u8(1);
        let key = source.root_key();
        let id = match w.source_ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = w.source_roots.len();
                w.source_ids.insert(key, id);
                w.source_roots.push(source.root());
                id
            }
        };
        let (start, end) = source.offsets();
        w.uv(id as u64);
        w.uv(start as u64);
        w.uv(end as u64);
    } else {
        w.u8(0);
    }
}

fn dec_opt_source(r: &mut Reader) -> R<Option<FunctionSource>> {
    if r.u8()? != 1 {
        return Ok(None);
    }
    let id = usize::try_from(r.uv()?).map_err(|_| "snapshot: source id overflow")?;
    let start = usize::try_from(r.uv()?).map_err(|_| "snapshot: source start overflow")?;
    let end = usize::try_from(r.uv()?).map_err(|_| "snapshot: source end overflow")?;
    let root = r.roots.get(id).ok_or("snapshot: missing source root")?;
    root.checked_range(start, end)
        .map(Some)
        .ok_or_else(|| "snapshot: invalid source range".into())
}

// ---- Stmt -------------------------------------------------------------------------------------

fn enc_stmt(w: &mut Writer, s: &Stmt) {
    if !w.enter() {
        return;
    }
    enc_stmt_inner(w, s);
    w.depth -= 1;
}
fn enc_stmt_inner(w: &mut Writer, s: &Stmt) {
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
            enc_opt_expr_ref(w, test.as_deref());
            enc_opt_expr_ref(w, update.as_deref());
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
            match handler.as_deref() {
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
    r.nested(dec_stmt_inner)
}

// Separate tag groups keep unoptimized stack frames small enough for the
// 128-level decoder limit on ordinary 2 MiB test/embedder threads.
fn dec_stmt_inner(r: &mut Reader) -> R<Stmt> {
    let tag = r.u8()?;
    match tag {
        0..=5 => dec_stmt_group_0(r, tag),
        6..=11 => dec_stmt_group_1(r, tag),
        12..=17 => dec_stmt_group_2(r, tag),
        18..=23 => dec_stmt_group_3(r, tag),
        24 => dec_stmt_group_4(r, tag),
        t => Err(format!("snapshot: bad Stmt tag {t}")),
    }
}

#[inline(never)]
fn dec_stmt_group_0(r: &mut Reader, tag: u8) -> R<Stmt> {
    Ok(match tag {
        0 => Stmt::Expr(dec_expr(r)?),
        1 => {
            let kind = dec_declkind(r)?;
            let n = r.count()?;
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
            let cons = P::new(dec_stmt(r)?);
            let alt = if r.u8()? == 1 {
                Some(P::new(dec_stmt(r)?))
            } else {
                None
            };
            Stmt::If { test, cons, alt }
        }
        5 => Stmt::Block(dec_stmts(r)?),
        _ => unreachable!("validated Stmt tag group"),
    })
}

#[inline(never)]
fn dec_stmt_group_1(r: &mut Reader, tag: u8) -> R<Stmt> {
    Ok(match tag {
        6 => Stmt::While {
            test: dec_expr(r)?,
            body: P::new(dec_stmt(r)?),
        },
        7 => Stmt::DoWhile {
            body: P::new(dec_stmt(r)?),
            test: dec_expr(r)?,
        },
        8 => {
            let init = if r.u8()? == 1 {
                Some(P::new(dec_forinit(r)?))
            } else {
                None
            };
            Stmt::For {
                init,
                test: dec_opt_expr(r)?.map(P::new),
                update: dec_opt_expr(r)?.map(P::new),
                body: P::new(dec_stmt(r)?),
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
                left: P::new(dec_pattern(r)?),
                right: dec_expr(r)?,
                of: r.bool()?,
                is_await: r.bool()?,
                body: P::new(dec_stmt(r)?),
            }
        }
        10 => Stmt::Break(dec_opt_str(r)?),
        11 => Stmt::Continue(dec_opt_str(r)?),
        _ => unreachable!("validated Stmt tag group"),
    })
}

#[inline(never)]
fn dec_stmt_group_2(r: &mut Reader, tag: u8) -> R<Stmt> {
    Ok(match tag {
        12 => Stmt::Throw(dec_expr(r)?),
        13 => {
            let block = dec_stmts(r)?;
            let handler = if r.u8()? == 1 {
                let param = if r.u8()? == 1 {
                    Some(dec_pattern(r)?)
                } else {
                    None
                };
                Some(P::new((param, dec_stmts(r)?)))
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
            let n = r.count()?;
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
            body: P::new(dec_stmt(r)?),
        },
        16 => Stmt::With {
            obj: dec_expr(r)?,
            body: P::new(dec_stmt(r)?),
        },
        17 => Stmt::ClassDecl(Rc::new(dec_class(r)?)),
        _ => unreachable!("validated Stmt tag group"),
    })
}

#[inline(never)]
fn dec_stmt_group_3(r: &mut Reader, tag: u8) -> R<Stmt> {
    Ok(match tag {
        18 => Stmt::Empty,
        19 => Stmt::Debugger,
        20 => {
            let source = r.rcstr()?;
            let n = r.count()?;
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
            let n = r.count()?;
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
        22 => Stmt::ExportDecl(P::new(dec_stmt(r)?)),
        23 => Stmt::ExportDefault(P::new(dec_stmt(r)?)),
        _ => unreachable!("validated Stmt tag group"),
    })
}

#[inline(never)]
fn dec_stmt_group_4(r: &mut Reader, tag: u8) -> R<Stmt> {
    Ok(match tag {
        24 => Stmt::ExportAll {
            source: r.rcstr()?,
            exported: dec_opt_str(r)?,
        },
        _ => unreachable!("validated Stmt tag group"),
    })
}

// ---- Expr -------------------------------------------------------------------------------------

fn enc_expr(w: &mut Writer, e: &Expr) {
    if !w.enter() {
        return;
    }
    enc_expr_inner(w, e);
    w.depth -= 1;
}
fn enc_expr_inner(w: &mut Writer, e: &Expr) {
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
        Expr::TaggedTemplate { tag, parts } => {
            w.u8(29);
            enc_expr(w, tag);
            w.uv(parts.quasis.len() as u64);
            for (cooked, raw) in parts.quasis.iter() {
                enc_opt_str(w, cooked);
                w.str(raw);
            }
            enc_exprs(w, &parts.subs);
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
    r.nested(dec_expr_inner)
}

fn dec_callee(r: &mut Reader) -> R<Expr> {
    r.nested(|r| dec_expr_inner_mode(r, true))
}

// Separate tag groups keep unoptimized stack frames small enough for the
// 128-level decoder limit on ordinary 2 MiB test/embedder threads.
fn dec_expr_inner(r: &mut Reader) -> R<Expr> {
    dec_expr_inner_mode(r, false)
}

fn dec_expr_inner_mode(r: &mut Reader, directly_called: bool) -> R<Expr> {
    let tag = r.u8()?;
    match tag {
        // An immediately invoked function needs its body as soon as this expression runs.
        // Decode it once now, while still deferring functions declared inside that body.
        0 if directly_called => Ok(Expr::Paren(P::new(dec_callee(r)?))),
        13 if directly_called => Ok(Expr::Func(Rc::new(
            r.nested(|r| dec_function_inner(r, false))?,
        ))),
        0..=5 => dec_expr_group_0(r, tag),
        6..=11 => dec_expr_group_1(r, tag),
        12..=17 => dec_expr_group_2(r, tag),
        18..=23 => dec_expr_group_3(r, tag),
        24..=29 => dec_expr_group_4(r, tag),
        30..=34 => dec_expr_group_5(r, tag),
        t => Err(format!("snapshot: bad Expr tag {t}")),
    }
}

#[inline(never)]
fn dec_expr_group_0(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        0 => Expr::Paren(P::new(dec_expr(r)?)),
        1 => Expr::Num(r.f64()?),
        2 => Expr::BigInt(JsBigInt::parse_radix(&r.str()?, 16).ok_or("snapshot: bad bigint")?),
        3 => Expr::Str(r.str_slice()?.into()),
        4 => Expr::ToStr(P::new(dec_expr(r)?)),
        5 => Expr::Bool(r.bool()?),
        _ => unreachable!("validated Expr tag group"),
    })
}

#[inline(never)]
fn dec_expr_group_1(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        6 => Expr::Null,
        7 => Expr::Undefined,
        8 => Expr::Ident(r.str_slice()?.into()),
        9 => Expr::This,
        10 => Expr::Regex {
            body: r.str_slice()?.into(),
            flags: r.str_slice()?.into(),
        },
        11 => Expr::Array(dec_array_elems(r)?),
        _ => unreachable!("validated Expr tag group"),
    })
}

#[inline(never)]
fn dec_expr_group_2(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        12 => {
            let n = r.count()?;
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
                Some(P::new(dec_expr(r)?))
            } else {
                None
            };
            Expr::Yield { delegate, arg }
        }
        16 => Expr::Await(P::new(dec_expr(r)?)),
        17 => Expr::Super,
        _ => unreachable!("validated Expr tag group"),
    })
}

#[inline(never)]
fn dec_expr_group_3(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        18 => Expr::Unary {
            op: intern_op(r.str_slice()?)?,
            arg: P::new(dec_expr(r)?),
        },
        19 => Expr::Update {
            op: intern_op(r.str_slice()?)?,
            prefix: r.bool()?,
            arg: P::new(dec_expr(r)?),
        },
        20 => Expr::Binary {
            op: intern_op(r.str_slice()?)?,
            left: P::new(dec_expr(r)?),
            right: P::new(dec_expr(r)?),
        },
        21 => Expr::Logical {
            op: intern_op(r.str_slice()?)?,
            left: P::new(dec_expr(r)?),
            right: P::new(dec_expr(r)?),
        },
        22 => Expr::Assign {
            op: intern_op(r.str_slice()?)?,
            target: P::new(dec_expr(r)?),
            value: P::new(dec_expr(r)?),
        },
        23 => Expr::Cond {
            test: P::new(dec_expr(r)?),
            cons: P::new(dec_expr(r)?),
            alt: P::new(dec_expr(r)?),
        },
        _ => unreachable!("validated Expr tag group"),
    })
}

#[inline(never)]
fn dec_expr_group_4(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        24 => Expr::Call {
            callee: P::new(dec_callee(r)?),
            args: dec_array_elems(r)?.into_boxed_slice(),
            optional: r.bool()?,
        },
        25 => Expr::New {
            callee: P::new(dec_callee(r)?),
            args: dec_array_elems(r)?.into_boxed_slice(),
        },
        26 => Expr::Member {
            obj: P::new(dec_expr(r)?),
            prop: r.str()?.into_boxed_str(),
            optional: r.bool()?,
        },
        27 => Expr::Index {
            obj: P::new(dec_expr(r)?),
            index: P::new(dec_expr(r)?),
            optional: r.bool()?,
        },
        28 => Expr::Seq(dec_exprs(r)?),
        29 => {
            let tag = P::new(dec_expr(r)?);
            let n = r.count()?;
            let mut quasis = Vec::with_capacity(n);
            for _ in 0..n {
                quasis.push((dec_opt_str(r)?, r.str()?));
            }
            Expr::TaggedTemplate {
                tag,
                parts: Box::new(TemplateParts {
                    quasis,
                    subs: dec_exprs(r)?.into_boxed_slice(),
                }),
            }
        }
        _ => unreachable!("validated Expr tag group"),
    })
}

#[inline(never)]
fn dec_expr_group_5(r: &mut Reader, tag: u8) -> R<Expr> {
    Ok(match tag {
        30 => Expr::OptionalChain(P::new(dec_expr(r)?)),
        31 => Expr::PrivateIn {
            name: r.str_slice()?.into(),
            obj: P::new(dec_expr(r)?),
        },
        32 => Expr::ImportCall {
            spec: P::new(dec_expr(r)?),
            phase: match r.u8()? {
                0 => ImportPhase::Evaluation,
                1 => ImportPhase::Source,
                2 => ImportPhase::Defer,
                t => return Err(format!("snapshot: bad ImportPhase {t}")),
            },
            options: if r.u8()? == 1 {
                Some(P::new(dec_expr(r)?))
            } else {
                None
            },
        },
        33 => Expr::ImportMeta,
        34 => Expr::NewTarget,
        _ => unreachable!("validated Expr tag group"),
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
    let n = r.count()?;
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
    if !w.enter() {
        return;
    }
    enc_pattern_inner(w, p);
    w.depth -= 1;
}
fn enc_pattern_inner(w: &mut Writer, p: &Pattern) {
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
    r.nested(dec_pattern_inner)
}

fn dec_pattern_inner(r: &mut Reader) -> R<Pattern> {
    Ok(match r.u8()? {
        0 => Pattern::Ident(r.str()?),
        1 => {
            let n = r.count()?;
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
            let n = r.count()?;
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
        3 => Pattern::Member(P::new(dec_expr(r)?)),
        t => return Err(format!("snapshot: bad Pattern {t}")),
    })
}

fn enc_function(w: &mut Writer, f: &Function) {
    if !w.enter() {
        return;
    }
    enc_function_inner(w, f);
    w.depth -= 1;
}
fn enc_function_inner(w: &mut Writer, f: &Function) {
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
    enc_opt_source(w, &f.source);
}
fn dec_function(r: &mut Reader) -> R<Function> {
    r.nested(|r| dec_function_inner(r, true))
}

fn dec_function_inner(r: &mut Reader, defer_body: bool) -> R<Function> {
    let name = dec_opt_str(r)?;
    let n = r.count()?;
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        params.push(Param {
            pattern: dec_pattern(r)?,
            default: dec_opt_expr(r)?,
            rest: r.bool()?,
        });
    }
    let body = if let Some(source) = r.source.filter(|_| defer_body) {
        let start = r.pos;
        let known_end = source.body_ends.borrow().get(&start).copied();
        if let Some(end) = known_end {
            r.pos = end;
        } else {
            validate::statements(r)?;
            r.record_body(start);
        }
        FunctionBody::deferred(DeferredBody {
            source: source.clone(),
            start,
            end: r.pos,
            depth: r.depth,
        })
    } else {
        dec_stmts(r)?.into()
    };
    let flags = r.u8()?;
    let source = dec_opt_source(r)?;
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
        fn_maps: OnceCell::new(),
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
    enc_opt_source(w, &c.source);
}
fn dec_class(r: &mut Reader) -> R<Class> {
    let name = dec_opt_str(r)?;
    let superclass = if r.u8()? == 1 {
        Some(P::new(dec_expr(r)?))
    } else {
        None
    };
    let n = r.count()?;
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
        source: dec_opt_source(r)?,
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
            let n = r.count()?;
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
    use super::{decode, decode_eager as eager, encode, Expr, Stmt, Writer, MAGIC, VERSION};

    #[test]
    fn source_table_stores_one_root_for_nested_functions_and_classes() {
        let input = "function first() { function nested() { return '🌙'; } return nested; }\nfunction second() { function later() { return 'é'; } return later; }\nclass Café { constructor() {} method() { return 1; } }";
        let bytes = crate::compile_snapshot(input).unwrap();
        let (_, _, roots) = super::decode_header(&bytes, None).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(&*roots[0].checked_range(0, input.len()).unwrap(), input);
        assert_eq!(
            bytes
                .windows(input.len())
                .filter(|part| *part == input.as_bytes())
                .count(),
            1
        );
        let lazy = decode(&bytes).unwrap();
        // Materialize later bodies first: their source references must not depend on
        // definitions encountered while materializing an earlier deferred body.
        for (index, expected) in [
            (1, "function later() { return 'é'; }"),
            (0, "function nested() { return '🌙'; }"),
        ] {
            let Stmt::FuncDecl(outer) = &lazy[index] else {
                panic!("outer")
            };
            assert!(!outer.body.is_materialized());
            let Stmt::FuncDecl(inner) = &outer.body[0] else {
                panic!("inner")
            };
            assert_eq!(inner.source.as_deref(), Some(expected));
        }
        assert_eq!(encode(&lazy), bytes);
        assert_roundtrips("const f = (a = function defaultValue() {}) => `${function hole() { return `${() => 2}`; }}`; class C { [(() => 'x')()]() {} }");
    }

    #[test]
    fn source_table_rejects_bad_offsets_utf8_and_ranges() {
        let input = "function f() { return '🌙'; }";
        let bytes = crate::compile_snapshot(input).unwrap();
        let (body_start, body_end, _) = super::decode_header(&bytes, None).unwrap();
        // This small function ends in Some(source), root 0, start 0, end input.len().
        assert_eq!(
            &bytes[body_end - 4..body_end],
            &[1, 0, 0, input.len() as u8]
        );
        let rejected = |damaged: &[u8]| {
            assert!(decode(damaged).is_err());
            assert!(eager(damaged).is_err());
        };
        for offset in [0, body_start - 1, bytes.len(), usize::MAX] {
            let mut damaged = bytes.clone();
            damaged[body_start - 8..body_start].copy_from_slice(&(offset as u64).to_le_bytes());
            rejected(&damaged);
        }
        let moon = input.find('🌙').unwrap() as u8;
        for (field, value) in [
            (3, 1),
            (2, moon + 1),
            (1, moon + 1),
            (2, input.len() as u8 + 1),
            (1, input.len() as u8 + 1),
        ] {
            let mut damaged = bytes.clone();
            damaged[body_end - field] = value;
            rejected(&damaged);
        }
        let mut damaged = bytes.clone();
        // Corrupt only the root, including text inside a deferred body.
        damaged[body_end + 2 + moon as usize] = 0xff;
        rejected(&damaged);
        let mut damaged = bytes.clone();
        damaged[body_end] = 127;
        rejected(&damaged);
        let mut old = Writer::new(None);
        old.uv(MAGIC as u64);
        old.uv(1);
        old.uv(0);
        rejected(&old.buf);
        assert!(super::encode_limited(
            &crate::parser::parse_script(input, false).unwrap_or_else(|e| panic!("{}", e.message)),
            bytes.len() - 1
        )
        .is_none());
    }

    #[test]
    fn function_source_survives_input_mutation_and_deferred_body_decoding() {
        let outer_text =
            "function outer(π) { /* 🌙 */ function inner() { return π; } return inner; }";
        let class_text = "class Café { constructor() {} method() { return '🌙'; } }";
        let mut input = format!("/* prefix é */ {outer_text}\n{class_text}");
        let body =
            crate::parser::parse_script(&input, false).unwrap_or_else(|e| panic!("{}", e.message));
        input.clear();
        input.push_str("overwritten caller source");
        drop(input);
        let encoded = encode(&body);
        let mut temporary = encoded.clone();
        let lazy = decode(&temporary).unwrap();
        temporary.fill(0xff);
        drop(temporary);
        for body in [&body, &lazy] {
            let Stmt::FuncDecl(outer) = &body[0] else {
                panic!("outer")
            };
            assert_eq!(outer.source.as_deref(), Some(outer_text));
            let Stmt::FuncDecl(inner) = &outer.body[0] else {
                panic!("inner")
            };
            assert_eq!(
                inner.source.as_deref(),
                Some("function inner() { return π; }")
            );
            let Stmt::ClassDecl(class) = &body[1] else {
                panic!("class")
            };
            assert_eq!(class.source.as_deref(), Some(class_text));
            assert_eq!(
                class.members[0].func.as_ref().unwrap().source.as_deref(),
                Some(class_text)
            );
            assert_eq!(
                class.members[1].func.as_ref().unwrap().source.as_deref(),
                Some("method() { return '🌙'; }")
            );
            assert_eq!(encode(body), encoded);
        }
    }

    #[test]
    fn unused_function_bodies_remain_encoded() {
        let bytes = crate::compile_snapshot(
            "function outer() { function inner() { return 3; } return inner; }",
        )
        .unwrap();
        let body = decode(&bytes).unwrap();
        let Stmt::FuncDecl(outer) = &body[0] else {
            panic!("outer function");
        };
        assert!(!outer.body.is_materialized());
        assert_eq!(outer.name.as_deref(), Some("outer"));
        assert!(!outer.body.is_materialized());
        assert_eq!(outer.body.len(), 2);
        let Stmt::FuncDecl(inner) = &outer.body[0] else {
            panic!("inner function");
        };
        assert!(!inner.body.is_materialized());
        assert_eq!(encode(&body), bytes);
        assert!(inner.body.is_materialized());
    }

    #[test]
    fn immediately_called_bodies_decode_once_but_nested_functions_stay_deferred() {
        let bytes = crate::compile_snapshot(
            "(function boot() { function unused() { return 3; } return 1; })()",
        )
        .unwrap();
        let body = decode(&bytes).unwrap();
        let Stmt::Expr(Expr::Call { callee, .. }) = &body[0] else {
            panic!("immediate call");
        };
        let Expr::Func(function) = &**callee else {
            panic!("function callee");
        };
        assert!(function.body.is_materialized());
        let Stmt::FuncDecl(unused) = &function.body[0] else {
            panic!("nested function");
        };
        assert!(!unused.body.is_materialized());
        assert_eq!(encode(&body), bytes);
    }

    #[test]
    fn deferred_validation_matches_eager_decoder_on_corrupt_inputs() {
        let bytes = crate::compile_snapshot(
            r#"
            async function outer(a = 3, ...rest) {
              const {x = 1, ...tail} = {x: a};
              function* g() { yield* [1, 2n, /x/u, ...rest]; }
              class C { #x = 2; get x() { return this.#x; } static { this.y = 4; } }
              try { for (let k in tail) { if (k) break; } }
              catch (error) { throw error; } finally { a += 1; }
              return tag`head${await a}tail`;
            }
            (function boot() { function unused() { return 3; } return 1; })();
        "#,
        )
        .unwrap();
        let mut damaged = bytes.clone();
        for index in 0..bytes.len() {
            for mask in [1, 0x20, 0x80, 0xff] {
                damaged[index] = bytes[index] ^ mask;
                let lazy = decode(&damaged);
                let original = eager(&damaged);
                assert_eq!(lazy.is_ok(), original.is_ok(), "byte {index}, mask {mask}");
                if let (Ok(lazy), Ok(original)) = (lazy, original) {
                    // Force every deferred body and compare all decoded fields.
                    assert_eq!(
                        encode(&lazy),
                        encode(&original),
                        "byte {index}, mask {mask}"
                    );
                }
            }
            damaged[index] = bytes[index];
        }
    }

    #[test]
    fn every_lexer_operator_can_be_reinterned() {
        for &op in crate::token::KEYWORDS
            .iter()
            .chain(crate::token::PUNCTUATORS)
        {
            assert_eq!(*super::intern_op(op).unwrap(), op);
        }
        for invalid in ["", "unknownKeyword", "@@@@", "+x", ">>>==", "é", "in "] {
            assert!(super::intern_op(invalid).is_err());
        }
    }

    /// `encode` and `decode` must be exact inverses: re-encoding a decoded tree reproduces the
    /// bytes. This catches every tag/field-order asymmetry (a *dropped* field is caught instead
    /// by the behavioral suites that run the decoded glue).
    fn assert_roundtrips(src: &str) {
        let blob = crate::compile_snapshot(src).unwrap_or_else(|e| panic!("compile {src:?}: {e}"));
        let ast = decode(&blob).unwrap_or_else(|e| panic!("decode {src:?}: {e}"));
        assert_eq!(blob, encode(&ast), "re-encode differs for: {src}");
        assert_eq!(
            blob,
            encode(&eager(&blob).unwrap()),
            "eager decode differs for: {src}"
        );
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
    #[test]
    fn malformed_snapshots_are_bounded_and_rejected() {
        let body = crate::parser::parse_script("function f(x) { return x+1; } f(2)", false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let bytes = encode(&body);
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "prefix {n}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut huge = Writer::new(None);
        huge.uv(MAGIC as u64);
        huge.uv(VERSION as u64);
        let offset_position = huge.buf.len();
        huge.bytes(&0u64.to_le_bytes());
        huge.uv(u64::MAX);
        let table_start = huge.buf.len();
        huge.uv(0);
        huge.buf[offset_position..offset_position + 8]
            .copy_from_slice(&(table_start as u64).to_le_bytes());
        assert!(super::decode_header(&huge.buf, None).is_ok());
        assert!(decode(&huge.buf).is_err());
        let mut nested = Stmt::Empty;
        for _ in 0..160 {
            nested = Stmt::Block(vec![nested]);
        }
        let nested = [nested];
        assert!(decode(&encode(&nested)).is_err());
        assert!(super::encode_limited(&nested, 1024 * 1024).is_none());
        assert!(super::encode_limited(&body, 4).is_none());
    }

    #[test]
    fn nesting_limit_is_safe_on_a_two_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                for depth in [128, 129] {
                    let mut statement = Stmt::Empty;
                    for _ in 1..depth {
                        statement = Stmt::Block(vec![statement]);
                    }
                    let bytes = encode(&[statement]);
                    assert_eq!(decode(&bytes).is_ok(), depth == 128, "blocks: {depth}");

                    // The enclosing expression statement also consumes one depth level.
                    let mut expression = crate::ast::Expr::Num(1.0);
                    for _ in 2..depth {
                        expression = crate::ast::Expr::Paren(crate::ast::P::new(expression));
                    }
                    let bytes = encode(&[Stmt::Expr(expression)]);
                    assert_eq!(decode(&bytes).is_ok(), depth == 128, "expressions: {depth}");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
