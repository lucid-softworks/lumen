//! Runtime values and the object model. Objects are `Rc<RefCell<Object>>` ([`Gc`]); there is no
//! real garbage collector yet (reference counting, so cycles leak — acceptable for the test262
//! loop). Properties are stored in insertion order in a small map.

use crate::ast::Function;
use crate::interpreter::{Env, Interp};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub type Gc = Rc<RefCell<Object>>;

/// A native (Rust-implemented) function. It can only throw (via `Err`), never break/return/continue,
/// so a plain `Result<Value, Value>` (Err = the thrown value) is the whole contract.
pub type NativeFn = fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

/// A native function that carries captured state, unlike the bare-`fn` [`NativeFn`]. The embedder
/// uses this to wrap host callbacks that need associated data a function pointer can't hold — e.g.
/// an N-API C callback together with its `void*` and module handle.
pub type NativeClosure = dyn Fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

/// The engine value. `repr(u8)` with fixed discriminants gives it a *defined* layout — tag byte
/// at offset 0, payload at offset 8 — which the JIT's inline fast paths read directly (see
/// `jit::layout` for the compile-time assertions). Tags 0..=4 are the trivially-copyable
/// variants (no refcount): the JIT may memcpy exactly those.
#[derive(Clone, Default)]
#[repr(u8)]
pub enum Value {
    #[default]
    Undefined = 0,
    /// The spec's EMPTY completion marker: produced only by *statement* evaluation (declarations
    /// and other value-less statements) so completion values thread per UpdateEmpty. Never a JS
    /// value — every engine boundary converts it to `Undefined` before a value escapes.
    Empty = 1,
    Null = 2,
    Bool(bool) = 3,
    Num(f64) = 4,
    /// BigInt, approximated with `i128` (exact within ±2^127; tests beyond that range fail rather
    /// than implementing arbitrary precision).
    BigInt(crate::bigint::JsBigInt) = 5,
    Str(crate::lstr::LStr) = 6,
    Sym(Rc<SymbolData>) = 7,
    Obj(Gc) = 8,
}

/// A unique Symbol. Identity is the `id` (every `Symbol()` call gets a fresh one); `description` is
/// the optional label. Well-known symbols (`Symbol.iterator`, …) are just pre-allocated instances.
pub struct SymbolData {
    pub id: u64,
    pub description: Option<Rc<str>>,
}

/// Byte offsets the JIT's inline property-cache templates read directly out of the object graph.
/// Every field is *measured at runtime* against the real types (never hardcoded); the std layouts
/// that aren't guaranteed — where a `Vec`'s data pointer sits, the `RcBox` header size, the
/// `Option<Gc>` niche — are located by probing and reported in `valid`. If `valid` is false the
/// JIT emits no inline caches and everything routes through the checked helper, so a future
/// libstd layout change degrades performance, never correctness.
///
/// All offsets are relative to the *stored* `Rc` pointer — the value in a `Value::Obj` payload and
/// in an `Option<Gc>` (proto) field, which points at the `RcBox` header (`{strong, weak, value}`),
/// NOT at `Rc::as_ptr` (which is the inner `value`, `rcbox_data` bytes further on). The inline
/// templates only ever have the stored pointer, so measuring from it is what makes them correct.
#[derive(Clone, Copy)]
pub struct JitLayout {
    /// Stored `Rc` pointer → the `Object` (through the `RcBox` header and the `RefCell` wrapper).
    pub obj_from_rc: usize,
    /// Stored `Rc` pointer → `Rc::as_ptr` (the RcBox header size): what the call probes add to
    /// a Value payload before comparing against a fill-time `Rc::as_ptr` identity.
    pub gc_data_off: usize,
    /// Stored `Rc` pointer → the strong count (the `RcBox`'s first field).
    pub rc_strong_off: usize,
    pub obj_proto: usize,
    pub obj_props: usize,
    pub obj_exotic: usize,
    pub props_shape: usize,
    /// The `entries` `Vec` within `Props`.
    pub props_entries: usize,
    /// The data-pointer word within a `Vec` (not necessarily offset 0 — RawVec layout is unstable).
    pub vec_ptr_off: usize,
    /// The length word within a `Vec` (probed like `vec_ptr_off`).
    pub vec_len_off: usize,
    /// The `elems` dense-element `Vec<u32>` within `Props`.
    pub props_elems: usize,
    /// The `mirror` raw-f64 element `Vec` within `Props` (see [`Props::mirror`]).
    pub props_mirror: usize,
    /// The `mirror_flags` byte within `Props`.
    pub props_mirror_flags: usize,
    /// `size_of::<(Rc<str>, Property)>()` — the entry stride.
    pub entry_size: usize,
    /// `Value` within an entry `(Rc<str>, Property)`.
    pub entry_value: usize,
    /// `accessor` bool within an entry.
    pub entry_accessor: usize,
    /// `writable` bool within an entry.
    pub entry_writable: usize,
    /// `Exotic::None`'s discriminant byte (the inline path requires an ordinary object).
    pub exotic_none_tag: u8,
    /// `Exotic::Array`'s discriminant byte (the element templates also accept arrays).
    pub exotic_array_tag: u8,
    /// `Exotic::StrWrap`'s discriminant byte (String.prototype IS a StrWrap — the GetMethod
    /// template accepts it as a named-read holder; index/length reads never take that path).
    pub exotic_strwrap_tag: u8,
    /// `ic_plain` byte within `Object` (the per-receiver "not in an exotic side table" flag).
    pub obj_ic_plain: usize,
    /// `Rc::as_ptr(env)` → the scope's `VarMap` generation counter (through the `RefCell`).
    pub scope_gen: usize,
    /// `value` within a `Binding` (the LoadName template's 24-byte copy source).
    pub binding_value: usize,
    /// `initialized` bool within a `Binding` (TDZ check).
    pub binding_init: usize,
    /// The `Rc<str>` key within an entry `(Rc<str>, Property)` (tuple field order is unstable).
    pub entry_key: usize,
    /// The length word within an `Rc<str>` fat pointer (0 or 8 — layout is unstable).
    pub str_len_word: usize,
    /// The pointer word within an `Rc<str>` fat pointer (the other one).
    pub str_ptr_word: usize,
    /// Stored `Rc<str>` pointer word → the first byte of the string data (the RcBox header).
    pub str_data_off: usize,
    /// Whether the four fields above probed successfully (key-checked array-holder entries can
    /// inline their key compare only when they did).
    pub key_probe_ok: bool,
    pub valid: bool,
}

/// Measure [`JitLayout`] against the live types, probing the non-guaranteed std layouts.
pub(crate) fn jit_layout(sample: &Gc) -> JitLayout {
    use std::mem::offset_of;
    // Rc<str> fat-pointer probe (word order and RcBox data offset are not std-guaranteed): a
    // known 8-byte string tells us which word holds the length; the data pointer is the other,
    // and `as_ptr` minus the stored word gives the RcBox header size. Fails closed.
    let (str_len_word, str_ptr_word, str_data_off, key_probe_ok) = {
        let probe: Rc<str> = "probe_8B".into();
        let words: [usize; 2] =
            unsafe { std::mem::transmute_copy::<Rc<str>, [usize; 2]>(&probe) };
        let data = probe.as_ptr() as usize;
        if words[0] == 8 && words[1] != 8 && data > words[1] && data - words[1] < 256 {
            (0usize, 8usize, data - words[1], true)
        } else if words[1] == 8 && words[0] != 8 && data > words[0] && data - words[0] < 256 {
            (8usize, 0usize, data - words[0], true)
        } else {
            (0, 0, 0, false)
        }
    };
    let as_ptr = Rc::as_ptr(sample) as usize; // → the RefCell<Object> (RcBox value field)
    let stored_word = unsafe { *(sample as *const Gc as *const usize) };
    let gc_data_off = as_ptr.wrapping_sub(stored_word);
    let obj_addr = &*sample.borrow() as *const Object as usize;
    let refcell_value = obj_addr - as_ptr;

    // The *stored* Rc pointer — what a Value::Obj payload / Option<Gc> holds — is the RcBox base
    // (strong count at its start), `rcbox_data` bytes before `Rc::as_ptr`. Read it out of an
    // Option<Gc> (whose Some variant is exactly the raw pointer, None = null via the niche).
    let some_proto: Option<Gc> = Some(sample.clone());
    let stored = unsafe { *(&some_proto as *const Option<Gc> as *const usize) };
    let none_proto: Option<Gc> = None;
    let none_word = unsafe { *(&none_proto as *const Option<Gc> as *const usize) };
    let niche_ok = none_word == 0 && as_ptr >= stored;
    let rcbox_data = as_ptr - stored; // RcBox header (strong+weak) before the value
    let obj_from_rc = rcbox_data + refcell_value; // stored ptr → Object
    let rc_strong_off = 0usize; // strong count is the RcBox's first field
                                // Verify: the strong count sits at `stored + rc_strong_off` and reads the live count.
    let strong_ok =
        unsafe { *((stored + rc_strong_off) as *const usize) } == Rc::strong_count(sample);

    // Vec data-pointer and length words (RawVec layout is not guaranteed — locate them by value).
    // Capacity 3 / length 1 makes the three words distinguishable.
    let mut v: Vec<(Rc<str>, Property)> = Vec::with_capacity(3);
    v.push((Rc::from("p"), Property::plain(Value::Num(0.0))));
    let vptr = v.as_ptr() as usize;
    let vwords = unsafe {
        std::slice::from_raw_parts(
            &v as *const Vec<_> as *const usize,
            std::mem::size_of::<Vec<(Rc<str>, Property)>>() / 8,
        )
    };
    let vec_ptr_off = vwords.iter().position(|&w| w == vptr).map(|i| i * 8);
    let vec_len_off = vwords.iter().position(|&w| w == 1).map(|i| i * 8);
    // The element templates index a `Vec<u32>` (`Props::elems`) with the same offsets; verify the
    // layout really is per-Vec-struct, not per-element-type.
    let mut v32: Vec<u32> = Vec::with_capacity(3);
    v32.push(7);
    let v32ptr = v32.as_ptr() as usize;
    let v32words = unsafe {
        std::slice::from_raw_parts(
            &v32 as *const Vec<u32> as *const usize,
            std::mem::size_of::<Vec<u32>>() / 8,
        )
    };
    let vec32_ok = vec_ptr_off.is_some_and(|o| v32words[o / 8] == v32ptr)
        && vec_len_off.is_some_and(|o| v32words[o / 8] == 1);

    // Exotic::None / Exotic::Array discriminants (Exotic is repr(Rust); probe to be certain).
    let none = Exotic::None;
    let exotic_none_tag = unsafe { *(&none as *const Exotic as *const u8) };
    let arr = Exotic::Array;
    let exotic_array_tag = unsafe { *(&arr as *const Exotic as *const u8) };
    let sw = Exotic::StrWrap("".into());
    let exotic_strwrap_tag = unsafe { *(&sw as *const Exotic as *const u8) };

    // Scope offsets for the inline LoadName template: Rc::as_ptr → RefCell<Scope> value →
    // Scope.vars → VarMap generation. The RefCell value offset is probed on a live scope.
    let probe_env = crate::interpreter::new_scope(None);
    let scope_addr = {
        let b = probe_env.borrow();
        &*b as *const crate::interpreter::Scope as usize
    };
    let scope_refcell = scope_addr - Rc::as_ptr(&probe_env) as usize;
    let scope_gen = scope_refcell
        + offset_of!(crate::interpreter::Scope, vars)
        + crate::interpreter::VarMap::generation_offset();
    let binding_value = offset_of!(crate::interpreter::Binding, value);
    let binding_init = offset_of!(crate::interpreter::Binding, initialized);

    let valid = strong_ok && niche_ok && vec_ptr_off.is_some() && vec_len_off.is_some() && vec32_ok;
    JitLayout {
        obj_from_rc,
        gc_data_off,
        rc_strong_off,
        obj_proto: offset_of!(Object, proto),
        obj_ic_plain: offset_of!(Object, ic_plain),
        obj_props: offset_of!(Object, props),
        obj_exotic: offset_of!(Object, exotic),
        props_shape: offset_of!(Props, shape),
        props_entries: offset_of!(Props, entries),
        vec_ptr_off: vec_ptr_off.unwrap_or(0),
        vec_len_off: vec_len_off.unwrap_or(0),
        props_elems: offset_of!(Props, elems),
        props_mirror: offset_of!(Props, mirror),
        props_mirror_flags: offset_of!(Props, mirror_flags),
        entry_size: std::mem::size_of::<(Rc<str>, Property)>(),
        entry_key: offset_of!((Rc<str>, Property), 0),
        str_len_word,
        str_ptr_word,
        str_data_off,
        key_probe_ok,
        entry_value: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, value),
        entry_accessor: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, accessor),
        entry_writable: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, writable),
        exotic_none_tag,
        exotic_array_tag,
        exotic_strwrap_tag,
        scope_gen,
        binding_value,
        binding_init,
        valid,
    }
}

impl Value {
    pub fn str(s: impl Into<crate::lstr::LStr>) -> Value {
        Value::Str(s.into())
    }
    pub fn from_string(s: String) -> Value {
        Value::Str(s.into())
    }
    /// A BigInt from an `i64` (for the embedder's 64-bit integer bridge, e.g. wasm i64).
    pub fn bigint_from_i64(v: i64) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from(v))
    }
    /// A BigInt from a `u64` (for the embedder's 64-bit bridge, e.g. an unsigned FFI return).
    pub fn bigint_from_u64(v: u64) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from_u64(v))
    }
    /// A BigInt from an `i128` (an FFI `int64_t` widened to preserve its sign).
    pub fn bigint_from_i128(v: i128) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from_i128(v))
    }
    /// Read a BigInt as an `i64` (wrapping past ±2^63), for the embedder's 64-bit bridge. `None`
    /// when the value isn't a BigInt.
    pub fn bigint_as_i64(&self) -> Option<i64> {
        match self {
            Value::BigInt(b) => Some(b.to_i128_wrapping() as i64),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Gc> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }
    /// The number, if this is a `Number` (an embedder convenience for reading op arguments).
    pub fn as_num_opt(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Obj(o) if !matches!(o.borrow().call, Callable::None))
    }
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined | Value::Empty => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::BigInt(_) => "bigint",
            Value::Str(_) => "string",
            Value::Sym(_) => "symbol",
            Value::Obj(o) => {
                if matches!(o.borrow().call, Callable::None) {
                    "object"
                } else {
                    "function"
                }
            }
        }
    }
}

/// How an object can be called. Most objects are not callable (`None`).
#[derive(Clone)]
pub enum Callable {
    None,
    Native(NativeFn),
    /// A native function carrying captured state (see [`NativeClosure`]).
    NativeData(std::rc::Rc<NativeClosure>),
    /// An interpreted function: its AST plus the lexical environment it closed over.
    User(Rc<Function>, Env),
    /// The result of `Function.prototype.bind`.
    Bound {
        target: Gc,
        this: Value,
        args: Vec<Value>,
    },
    /// A ShadowRealm wrapped function: `target` is a callable inside the sub-realm identified by
    /// `realm` (its pointer). Calls marshal primitive args in and the primitive result out.
    WrappedShadow {
        realm: usize,
        target: Box<Value>,
    },
    /// The inverse: a function living *inside* a ShadowRealm whose `target` is a callable of the
    /// host realm. `realm` is this sub-realm's key in the host's map and `parent` is the host
    /// interpreter's stable address (hosts are either the engine root or boxed sub-realms, both
    /// pinned in memory while any of their sub-realm objects exist).
    WrappedCross {
        realm: usize,
        parent: usize,
        target: Box<Value>,
    },
    /// An auto-accessor's synthesized getter: reads the private backing field (brand-checked) off
    /// the receiver.
    AccessorGet(Rc<str>),
    /// An auto-accessor's synthesized setter: writes the private backing field (brand-checked).
    AccessorSet(Rc<str>),
    /// A decorator `context.access.get`: returns `args[0][name]`.
    PropGet(Rc<str>),
    /// A decorator `context.access.set`: performs `args[0][name] = args[1]`.
    PropSet(Rc<str>),
}

/// Exotic internal data for built-in object kinds (arrays, primitive wrappers). The wrapper
/// variants are read by the `this_*` coercion helpers but not yet constructed (`new String()` etc.
/// still return primitives — boxing is the next built-ins milestone).
#[derive(Clone)]
#[allow(dead_code)]
pub enum Exotic {
    None,
    Array,
    BoolWrap(bool),
    NumWrap(f64),
    StrWrap(crate::lstr::LStr),
    SymWrap(Rc<SymbolData>),
    BigIntWrap(crate::bigint::JsBigInt),
    /// An error object. Carries the captured call-stack frames as a preformatted string (the
    /// `\n    at <fn>` lines, empty when thrown at top level), snapshotted at construction; the
    /// `Error.prototype.stack` getter prepends the live `name: message` head. name/message live as
    /// ordinary properties, and the tag lets `Error.prototype.toString` / the test262 runner
    /// recognise an error cheaply.
    Error(Rc<str>),
    /// An `arguments` exotic object (mapped index/parameter aliasing lives in
    /// `Interp::mapped_arguments`).
    Arguments,
}

pub struct Object {
    pub(crate) proto: Option<Gc>,
    pub(crate) props: Props,
    pub(crate) extensible: bool,
    pub(crate) call: Callable,
    pub(crate) exotic: Exotic,
    /// `false` for objects whose behavior lives in an interpreter side table — proxies, typed
    /// arrays, module namespaces — which the `exotic` tag can't reveal. The JIT's inline
    /// property/element caches check this byte on the receiver and take the checked helper when
    /// clear, so ONE proxy existing somewhere no longer disables the caches for every plain
    /// object in the program (the old global `inline_ic_safe` latch).
    pub(crate) ic_plain: Cell<bool>,
    /// The construct-time prototype handed to instances (`F.prototype`), cached for `new`.
    pub(crate) is_constructor: bool,
    /// GC scratch: mark bit (reachability) and a count of references from other heap objects.
    pub(crate) gc_mark: Cell<bool>,
    pub(crate) gc_internal: Cell<u32>,
}

impl Object {
    pub(crate) fn new(proto: Option<Gc>) -> Gc {
        LIVE_OBJECTS.with(|c| c.set(c.get() + 1));
        let obj = Rc::new(RefCell::new(Object {
            proto,
            props: Props::new(),
            extensible: true,
            call: Callable::None,
            exotic: Exotic::None,
            ic_plain: Cell::new(true),
            is_constructor: false,
            gc_mark: Cell::new(false),
            gc_internal: Cell::new(0),
        }));
        GC_REGISTRY.with(|r| {
            let mut reg = r.borrow_mut();
            reg.entries.push(Rc::downgrade(&obj));
            // A dead Weak still owns the RcBox allocation. Workloads with heavy object churn but
            // a small live set may never arm the cycle collector, so prune the weak registry on
            // its own volume instead of allowing millions of dead boxes to accumulate.
            if reg.entries.len() > reg.next_prune {
                reg.entries.retain(|w| w.strong_count() > 0);
                reg.next_prune = reg
                    .entries
                    .len()
                    .saturating_mul(2)
                    .max(GC_REGISTRY_PRUNE_TRIGGER);
            }
        });
        obj
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // `try_with` so a drop during thread-local teardown at process exit can't panic.
        let _ = LIVE_OBJECTS.try_with(|c| c.set(c.get() - 1));
    }
}

// The GC is a refcount-based cycle collector (lumen has no tracing GC). Every heap object is
// registered (as a Weak) and the live count is maintained via Object::new / Drop. `Interp::gc_collect`
// reclaims objects referenced only by other (also-unreachable) objects — see interpreter.rs.
const GC_REGISTRY_PRUNE_TRIGGER: usize = 65_536;

struct GcRegistry {
    entries: Vec<Weak<RefCell<Object>>>,
    next_prune: usize,
}

thread_local! {
    static GC_REGISTRY: RefCell<GcRegistry> = const {
        RefCell::new(GcRegistry {
            entries: Vec::new(),
            next_prune: GC_REGISTRY_PRUNE_TRIGGER,
        })
    };
    static LIVE_OBJECTS: Cell<i64> = const { Cell::new(0) };
}

/// Number of live heap objects right now.
pub fn live_objects() -> i64 {
    LIVE_OBJECTS.with(|c| c.get())
}

/// Strong handles to every currently-live heap object, pruning dead registry entries in passing.
pub fn gc_snapshot() -> Vec<Gc> {
    GC_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        let mut live = Vec::with_capacity(reg.entries.len());
        reg.entries.retain(|w| match w.upgrade() {
            Some(o) => {
                live.push(o);
                true
            }
            None => false,
        });
        reg.next_prune = reg
            .entries
            .len()
            .saturating_mul(2)
            .max(GC_REGISTRY_PRUNE_TRIGGER);
        live
    })
}

/// The element type of a TypedArray.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaKind {
    I8,
    U8,
    U8Clamped,
    I16,
    U16,
    I32,
    U32,
    F16,
    F32,
    F64,
    I64,
    U64,
}

impl TaKind {
    pub(crate) fn elsize(self) -> usize {
        match self {
            TaKind::I8 | TaKind::U8 | TaKind::U8Clamped => 1,
            TaKind::I16 | TaKind::U16 | TaKind::F16 => 2,
            TaKind::I32 | TaKind::U32 | TaKind::F32 => 4,
            TaKind::F64 | TaKind::I64 | TaKind::U64 => 8,
        }
    }
    /// Whether elements are BigInt (BigInt64Array / BigUint64Array) rather than Number.
    pub(crate) fn is_bigint(self) -> bool {
        matches!(self, TaKind::I64 | TaKind::U64)
    }
    /// Constructor / prototype name, e.g. "Int8Array".
    pub(crate) fn name(self) -> &'static str {
        match self {
            TaKind::I8 => "Int8Array",
            TaKind::U8 => "Uint8Array",
            TaKind::U8Clamped => "Uint8ClampedArray",
            TaKind::I16 => "Int16Array",
            TaKind::U16 => "Uint16Array",
            TaKind::I32 => "Int32Array",
            TaKind::U32 => "Uint32Array",
            TaKind::F16 => "Float16Array",
            TaKind::F32 => "Float32Array",
            TaKind::F64 => "Float64Array",
            TaKind::I64 => "BigInt64Array",
            TaKind::U64 => "BigUint64Array",
        }
    }
    /// Read a BigInt element (little-endian) from `b` (8 bytes) as an i128.
    pub(crate) fn read_bigint(self, b: &[u8]) -> i128 {
        let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match self {
            TaKind::U64 => u64::from_le_bytes(arr) as i128,
            _ => i64::from_le_bytes(arr) as i128,
        }
    }
    /// Convert a BigInt (i128) to this element's 8 little-endian bytes, wrapping mod 2^64.
    pub(crate) fn write_bigint(self, n: i128) -> Vec<u8> {
        (n as u64).to_le_bytes().to_vec()
    }
    /// Read one element (little-endian) from `b` (which must be `elsize()` bytes) as a Number.
    pub(crate) fn read(self, b: &[u8]) -> f64 {
        match self {
            TaKind::I8 => b[0] as i8 as f64,
            TaKind::U8 | TaKind::U8Clamped => b[0] as f64,
            TaKind::I16 => i16::from_le_bytes([b[0], b[1]]) as f64,
            TaKind::U16 => u16::from_le_bytes([b[0], b[1]]) as f64,
            TaKind::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::U32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::F16 => f16_to_f32(u16::from_le_bytes([b[0], b[1]])) as f64,
            TaKind::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::F64 => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            TaKind::I64 | TaKind::U64 => self.read_bigint(b) as f64,
        }
    }
    /// Convert a Number to this element type's little-endian bytes (JS integer-conversion rules).
    pub(crate) fn write(self, n: f64) -> Vec<u8> {
        let int = |n: f64| if n.is_finite() { n.trunc() as i64 } else { 0 };
        match self {
            TaKind::I8 => vec![int(n) as i8 as u8],
            TaKind::U8 => vec![int(n) as u8],
            TaKind::U8Clamped => {
                // ToUint8Clamp: round-half-to-even (0.5 → 0, 1.5 → 2, 2.5 → 2), clamped to [0,255].
                let c = if n.is_nan() || n <= 0.0 {
                    0.0
                } else if n >= 255.0 {
                    255.0
                } else {
                    let f = n.floor();
                    if f + 0.5 < n {
                        f + 1.0
                    } else if n < f + 0.5 {
                        f
                    } else if (f as i64) % 2 == 1 {
                        f + 1.0
                    } else {
                        f
                    }
                };
                vec![c as u8]
            }
            TaKind::I16 => (int(n) as i16).to_le_bytes().to_vec(),
            TaKind::U16 => (int(n) as u16).to_le_bytes().to_vec(),
            TaKind::I32 => (int(n) as i32).to_le_bytes().to_vec(),
            TaKind::U32 => (int(n) as u32).to_le_bytes().to_vec(),
            TaKind::F16 => f64_to_f16(n).to_le_bytes().to_vec(),
            TaKind::F32 => (n as f32).to_le_bytes().to_vec(),
            TaKind::F64 => n.to_le_bytes().to_vec(),
            TaKind::I64 | TaKind::U64 => self.write_bigint(int(n) as i128),
        }
    }
}

/// A TypedArray view's internal state (the engine's `[[ViewedArrayBuffer]]`/`[[ByteOffset]]`/
/// `[[ArrayLength]]`/`[[TypedArrayName]]`). Stored in an `Interp` side table keyed by object ptr.
#[derive(Clone, Copy)]
pub struct TaInfo {
    /// Pointer of the backing ArrayBuffer object (key into `Interp::array_buffers`).
    pub buffer: usize,
    pub offset: usize,
    pub len: usize,
    pub kind: TaKind,
    /// Length-tracking view (created on a resizable buffer with no explicit length): its length is
    /// recomputed from the buffer's current size rather than fixed at `len`.
    pub track: bool,
}

/// How a property key relates to a TypedArray's integer-indexed exotic behavior.
pub enum TaIndex {
    /// A valid in-range element index.
    Element(usize),
    /// A canonical numeric key that isn't a valid index (inert: get→undefined, set/define→no-op,
    /// has→false, delete→true; never stored, never reaches the prototype).
    Exotic,
    /// An ordinary string/symbol key (handled by the normal property machinery).
    Ordinary,
}

/// A property descriptor. A data property uses `value`/`writable`; an accessor uses the boxed
/// getter/setter pair. Data properties vastly outnumber accessors, so the pair lives behind one
/// pointer: Property is 40 bytes instead of 80, and a `Props` entry 56 instead of 96 — the
/// entries array the interpreter chases on every IC hit holds nearly twice the properties per
/// cache line.
#[derive(Clone)]
pub struct Property {
    pub value: Value,
    acc: Option<Box<Accessors>>,
    pub accessor: bool,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

/// The boxed getter/setter pair of an accessor property.
#[derive(Clone, Default)]
pub(crate) struct Accessors {
    pub get: Option<Value>,
    pub set: Option<Value>,
}

impl Property {
    pub(crate) fn data(
        value: Value,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        Property {
            value,
            acc: None,
            accessor: false,
            writable,
            enumerable,
            configurable,
        }
    }
    /// An accessor property (`accessor: true`, value `Undefined`, not writable).
    pub(crate) fn accessor_prop(
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        Property {
            value: Value::Undefined,
            acc: Some(Box::new(Accessors { get, set })),
            accessor: true,
            writable: false,
            enumerable,
            configurable,
        }
    }
    #[inline]
    pub(crate) fn getter(&self) -> Option<&Value> {
        self.acc.as_ref().and_then(|a| a.get.as_ref())
    }
    #[inline]
    pub(crate) fn setter(&self) -> Option<&Value> {
        self.acc.as_ref().and_then(|a| a.set.as_ref())
    }
    pub(crate) fn set_getter(&mut self, g: Option<Value>) {
        match (&mut self.acc, g) {
            (Some(a), g) => a.get = g,
            (acc @ None, Some(g)) => {
                *acc = Some(Box::new(Accessors {
                    get: Some(g),
                    set: None,
                }))
            }
            (None, None) => {}
        }
    }
    pub(crate) fn set_setter(&mut self, s: Option<Value>) {
        match (&mut self.acc, s) {
            (Some(a), s) => a.set = s,
            (acc @ None, Some(s)) => {
                *acc = Some(Box::new(Accessors {
                    get: None,
                    set: Some(s),
                }))
            }
            (None, None) => {}
        }
    }
    /// Drop the accessor pair (used when a define converts an accessor back to a data property).
    pub(crate) fn clear_accessors(&mut self) {
        self.acc = None;
    }
    /// A default plain data property: writable, enumerable, configurable.
    pub(crate) fn plain(value: Value) -> Property {
        Property::data(value, true, true, true)
    }
    /// A non-enumerable method/builtin property: writable + configurable, not enumerable.
    pub(crate) fn builtin(value: Value) -> Property {
        Property::data(value, true, false, true)
    }
}

/// Insertion-ordered string-keyed property map. A `Vec` of entries preserves order (good enough for
/// `for-in`/`Object.keys`); a side `HashMap` keeps lookup O(1).
#[derive(Clone)]
pub struct Props {
    entries: Vec<(Rc<str>, Property)>,
    index: crate::fasthash::FastMap<Rc<str>, usize>,
    /// This object serves (or once served) as some object's prototype: structural changes to it
    /// bump the global [`proto_epoch`], invalidating every property-*creation* inline cache
    /// (their fill-time chain walks proved "no hop shadows this name" — see
    /// [`crate::bytecode::IC_CREATE`]). Set by the creation-IC fill walk itself, one-way.
    proto_flag: std::cell::Cell<bool>,
    /// Object shape (hidden class): the id encoding this map's ordered key sequence (see
    /// [`ShapeTable`]). Two `Props` share an id exactly when they added the same keys in the same
    /// order, so an inline cache that recorded (shape, slot) from one object can trust that slot
    /// on any other object of the same shape — without a key compare. Bumped to a child on
    /// new-key insert, to a fresh unique on a structural removal. Only consulted for non-exotic
    /// objects (arrays keep the key-compare path — same shape can mean different element counts).
    shape: u32,
    /// Dense element map: `elems[n]` is the `entries` slot of canonical-index key `n`, or
    /// `NO_SLOT`. Maintained for a (near-)contiguous prefix from 0 — a canonical key far past the
    /// dense frontier lives only in `index` (see `note_inserted`). This is what makes `a[i]`
    /// O(1) without hashing or stringifying the index (see `get_index`).
    elems: Vec<u32>,
    /// Raw-f64 read mirror of the dense elements. While `mirror_flags & MIRROR_OK`:
    /// `mirror.len() == elems.len()`, and for every `n`: `mirror[n]` is [`MIRROR_HOLE`] exactly
    /// when `elems[n]` names no element, else the element is a plain writable data property
    /// whose value is `Num(mirror[n])`. Element reads become one indexed load (no entry chase,
    /// no tag check), and `MIRROR_ALL_I32` lets the JIT's int loops skip the exactness guard
    /// entirely. Entries stay authoritative: fast writers dual-store through
    /// [`Props::set_index_value`]; any foreign `&mut` escape (`get_index_mut`, `get_mut` /
    /// `entry_at_mut` on an index key) invalidates the mirror instead of tracking it.
    mirror: Vec<f64>,
    /// [`MIRROR_OK`] | [`MIRROR_ALL_I32`] | [`MIRROR_NO_HOLES`].
    mirror_flags: u8,
    /// Live hole count in `mirror` (descending array fills pad with holes and then fill them:
    /// `MIRROR_NO_HOLES` comes back when this returns to zero).
    mirror_holes: u32,
    /// Some canonical-index key lives ONLY in the string-keyed map (inserted too far past the
    /// dense frontier — see `note_inserted`): `elems` coverage is no longer proof of element
    /// absence, so the dense append/pop fast paths stand down. One-way (sparse arrays are rare
    /// and stay sparse).
    has_far: std::cell::Cell<bool>,
    /// This `Props` belongs to an `Exotic::Array` object: canonical-index key inserts skip the
    /// shape transition. Array shapes encode the *named*-key sequence only (matching
    /// `push_dense`, which never transitioned) — the get-IC only ever uses an array's shape to
    /// prove a named key's ABSENCE or with a per-hit key re-check, never for bare slot trust,
    /// so elements must not churn it: a stable shape is what lets `arr.push(..)`/`arr.length`
    /// sites cache at all.
    elem_mode: std::cell::Cell<bool>,
    /// The `entries` slot of the `"prototype"` key, or `NO_SLOT` — same memo discipline as
    /// `len_slot`. Every `new` reads the constructor's `.prototype`; function objects are
    /// ordinary maps, so this skips the scan on the construct hot path.
    proto_slot: std::cell::Cell<u32>,
    /// The `entries` slot of the `"length"` key, or `NO_SLOT`. Array `length` can't live in the
    /// inline caches (element entries occupy slots without transitioning the shape, so a shape
    /// match doesn't pin the slot) — this memo makes the every-time re-derive a direct slot read
    /// instead of a hashed key lookup. Maintained by `insert`; any slot-shifting removal resets
    /// it (`remove` re-memoizes on the next lookup via `length_slot`).
    len_slot: std::cell::Cell<u32>,
}

/// See [`Props::mirror`]. Bit values are chosen so the masks the JIT tests (`OK|NO_HOLES` and
/// `OK|NO_HOLES|ALL_I32`) are contiguous — encodable ARM64 logical immediates.
pub(crate) const MIRROR_OK: u8 = 1;
pub(crate) const MIRROR_NO_HOLES: u8 = 2;
/// Every non-hole mirror value is an exact i32 (bit-identical through an i32 round trip, which
/// also excludes -0.0).
pub(crate) const MIRROR_ALL_I32: u8 = 4;
/// The mirror's hole sentinel: a quiet-NaN payload no arithmetic produces. A user CAN craft
/// this exact bit pattern (typed-array punning), so the write paths refuse to mirror it — it is
/// never stored as data, which is what makes reading it back as "absent" sound.
pub(crate) const MIRROR_HOLE: u64 = 0x7FF8_DEAD_0000_0001;

/// Exact-i32 (and not -0.0): the value survives an i32 round trip bit-identically.
#[inline]
pub(crate) fn f64_exact_i32(f: f64) -> bool {
    (f as i32 as f64).to_bits() == f.to_bits()
}

/// `elems` hole marker (also caps how many entries dense slots can address).
const NO_SLOT: u32 = u32::MAX;

/// The empty-object shape: every `Props` starts here and all empty objects share it, so adding
/// the same first key to two of them lands on the same child shape.
const SHAPE_EMPTY: u32 = 0;

/// The property-creation epoch (see [`Props::proto_flag`]): bumped whenever a marked prototype
/// mutates structurally, any `[[SetPrototypeOf]]` succeeds, or a `defineProperty` rewrites
/// attributes — every event that could shadow a creation IC's "the chain has no setter /
/// non-writable / own copy of this name" proof. Process-global and atomic, NOT thread-local:
/// generator/async bodies run JS on pooled worker threads sharing the same `Interp` (one thread
/// at a time via channel handoff, which also orders these accesses), so a bump from a worker
/// must be visible to caches validated on the main thread. Starts at 1; saturates at `u32::MAX`,
/// which no cache hit accepts — after ~4e9 invalidations the creation ICs simply turn off
/// instead of ABA-cycling.
static PROTO_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// The current creation-IC epoch. `u32::MAX` = permanently invalidated (see [`PROTO_EPOCH`]).
#[inline]
pub(crate) fn proto_epoch() -> u32 {
    PROTO_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Invalidate every property-creation inline cache (see [`PROTO_EPOCH`]).
pub(crate) fn bump_proto_epoch() {
    let _ = PROTO_EPOCH.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| Some(v.saturating_add(1)),
    );
}

/// The object-shape (hidden-class) transition tree. A shape id encodes an *ordered sequence of
/// property keys* — two `Props` share an id exactly when they added the same keys in the same
/// order (attributes are NOT encoded; the inline cache re-checks accessor/writable at the slot).
/// `transitions[(parent, key)] = child` is memoized, so structurally-identical objects converge
/// on one id — which is what makes a shared per-site cache's shape compare meaningful (the flaw
/// that sank the earlier per-object version counter). A structural *removal* can't be a tree
/// transition (it doesn't extend the key sequence), so it mints a fresh unique id that no cache
/// ever holds — forcing a re-derive.
struct ShapeTable {
    transitions: crate::fasthash::FastMap<(u32, Rc<str>), u32>,
    next: u32,
    /// This thread's id-range base (see `SHAPE_ORDINAL`).
    base: u32,
}

/// Allocates each thread's shape-id range. The table itself is thread-local (its `Rc<str>` keys
/// can't cross threads), but generator/async bodies run JS on pooled *worker* threads sharing
/// the same `Interp` and object graph — so ids minted on different threads flow through the same
/// inline caches and MUST NOT collide. Each thread takes a disjoint `ordinal << 24` range
/// (16.7M shapes per thread; the coroutine pool keeps the thread count small — past 256 threads
/// ordinals recycle, restoring the pre-partitioning collision odds rather than failing).
static SHAPE_ORDINAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

thread_local! {
    static SHAPES: RefCell<ShapeTable> = RefCell::new({
        let ord = SHAPE_ORDINAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xFF;
        let base = ord << 24;
        ShapeTable {
            transitions: Default::default(),
            next: base | 1, // low id 0 is skipped everywhere (SHAPE_EMPTY is the global 0)
            base,
        }
    });
}

impl ShapeTable {
    fn fresh(&mut self) -> u32 {
        let id = self.next;
        // Wrap within this thread's 24-bit range, skipping low-word 0 (SHAPE_EMPTY must stay
        // the empty object's id alone).
        let low = (id.wrapping_add(1)) & 0x00FF_FFFF;
        self.next = self.base | if low == 0 { 1 } else { low };
        id
    }
}

/// The child shape reached by adding `key` to shape `parent` (memoized so it is shared).
fn shape_transition(parent: u32, key: &Rc<str>) -> u32 {
    SHAPES.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&c) = t.transitions.get(&(parent, key.clone())) {
            return c;
        }
        let child = t.fresh();
        t.transitions.insert((parent, key.clone()), child);
        child
    })
}

/// A fresh unique shape id (a structural removal / deopt — no cache should still match).
fn shape_fresh() -> u32 {
    SHAPES.with(|t| t.borrow_mut().fresh())
}

/// Entry count up to which a `Props` runs without a hash index (linear-scan lookups, no hash
/// allocation or rehash on insert). Most objects — instance fields, cons cells, literals — stay
/// under it for their whole life.
const INDEX_THRESHOLD: usize = 8;

thread_local! {
    /// Interned key strings for small array indices — every dense array element key "0".."63"
    /// shares one allocation per thread instead of allocating per element.
    static INDEX_KEYS: Vec<Rc<str>> = (0..64).map(|i| Rc::from(i.to_string().as_str())).collect();
    /// Interned keys for the properties every function object carries — closure creation in a
    /// hot loop would otherwise allocate each key string per closure.
    static FN_KEYS: [Rc<str>; 4] = [
        Rc::from("length"),
        Rc::from("name"),
        Rc::from("prototype"),
        Rc::from("constructor"),
    ];
}

/// The property key for array index `n`, interned for small `n`.
pub(crate) fn index_key(n: usize) -> Rc<str> {
    if n < 64 {
        INDEX_KEYS.with(|k| k[n].clone())
    } else {
        Rc::from(n.to_string().as_str())
    }
}

/// Interned `"length"` / `"name"` / `"prototype"` / `"constructor"` keys (see `FN_KEYS`).
pub(crate) fn fn_key(i: usize) -> Rc<str> {
    FN_KEYS.with(|k| k[i].clone())
}

impl std::fmt::Debug for Props {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Props")
            .field("entries", &self.entries.len())
            .field("shape", &self.shape)
            .finish()
    }
}

impl Default for Props {
    fn default() -> Self {
        Self::new()
    }
}

impl Props {
    pub(crate) fn new() -> Props {
        Props {
            entries: Vec::new(),
            index: Default::default(),
            shape: SHAPE_EMPTY,
            elems: Vec::new(),
            mirror: Vec::new(),
            mirror_flags: MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES,
            mirror_holes: 0,
            proto_flag: std::cell::Cell::new(false),
            has_far: std::cell::Cell::new(false),
            elem_mode: std::cell::Cell::new(false),
            proto_slot: std::cell::Cell::new(NO_SLOT),
            len_slot: std::cell::Cell::new(NO_SLOT),
        }
    }

    /// Mark this map as an array's (see `elem_mode`). One-way, set when the owning object
    /// becomes `Exotic::Array`.
    #[inline]
    pub(crate) fn mark_array(&self) {
        self.elem_mode.set(true);
    }

    /// The `"length"` property, resolved through the `len_slot` memo (one compare, no hashing).
    /// `None` when there is no own `length`.
    pub(crate) fn length_property(&self) -> Option<&Property> {
        let s = self.len_slot.get();
        if s != NO_SLOT {
            debug_assert!(matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "length"));
            return self.entries.get(s as usize).map(|(_, p)| p);
        }
        let slot = self.find("length")?;
        self.len_slot.set(slot as u32);
        Some(&self.entries[slot].1)
    }

    /// Mark this object as a live prototype (see `proto_flag`).
    #[inline]
    pub(crate) fn mark_proto(&self) {
        self.proto_flag.set(true);
    }

    /// Bump the creation-IC epoch if this object is a marked prototype (called by every
    /// structural mutation).
    #[inline]
    fn note_structural(&self) {
        if self.proto_flag.get() {
            bump_proto_epoch();
        }
    }

    /// This map's shape id — the inline cache's structural validation token (see the `shape` field).
    #[inline]
    pub(crate) fn shape(&self) -> u32 {
        self.shape
    }

    /// The own property for canonical index `n`, without hashing. `None` only means "not in the
    /// dense map" — the caller must fall back to the string-keyed path, not conclude absence.
    #[inline]
    pub(crate) fn get_index(&self, n: u32) -> Option<&Property> {
        let slot = *self.elems.get(n as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        Some(&self.entries[slot as usize].1)
    }

    /// Mutable [`get_index`].
    #[inline]
    pub(crate) fn get_index_mut(&mut self, n: u32) -> Option<&mut Property> {
        // A raw &mut escape can rewrite the value behind the mirror's back.
        self.mirror_invalidate();
        let slot = *self.elems.get(n as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        Some(&mut self.entries[slot as usize].1)
    }

    /// Drop the element mirror (a foreign mutable escape or an unmirrorable element).
    #[inline]
    pub(crate) fn mirror_invalidate(&mut self) {
        if self.mirror_flags & MIRROR_OK != 0 {
            self.mirror_flags = 0;
            self.mirror = Vec::new();
        }
    }

    /// Re-mirror element `n` from `entries[slot]` (both already linked via `elems`).
    /// `filled_hole` = position `n` had no element before this (structural — a *data* value
    /// that happens to equal the hole sentinel must not confuse the accounting).
    fn mirror_sync(&mut self, n: usize, slot: usize, filled_hole: bool) {
        if self.mirror_flags & MIRROR_OK == 0 {
            return;
        }
        if self.mirror.len() != self.elems.len() {
            // Lockstep was broken by a path this code doesn't know — fail safe.
            self.mirror_invalidate();
            return;
        }
        let p = &self.entries[slot].1;
        match &p.value {
            Value::Num(f) if !p.accessor && p.writable && f.to_bits() != MIRROR_HOLE => {
                let f = *f;
                if !f64_exact_i32(f) {
                    self.mirror_flags &= !MIRROR_ALL_I32;
                }
                if filled_hole {
                    self.mirror_holes -= 1;
                    if self.mirror_holes == 0 {
                        self.mirror_flags |= MIRROR_NO_HOLES;
                    }
                }
                self.mirror[n] = f;
            }
            _ => self.mirror_invalidate(),
        }
    }

    /// Grow the mirror alongside `elems` with `pads` holes plus one freshly-linked element.
    fn mirror_grow(&mut self, pads: usize, slot: usize) {
        if self.mirror_flags & MIRROR_OK == 0 {
            return;
        }
        // Peek the value first: an object-element array (its very first push, typically) must
        // not pay a buffer allocation just to invalidate it.
        {
            let p = &self.entries[slot].1;
            let ok = matches!(&p.value, Value::Num(f) if f.to_bits() != MIRROR_HOLE)
                && !p.accessor
                && p.writable;
            if !ok {
                self.mirror_invalidate();
                return;
            }
        }
        if pads > 0 {
            self.mirror_flags &= !MIRROR_NO_HOLES;
            self.mirror_holes += pads as u32;
            self.mirror
                .extend(std::iter::repeat(f64::from_bits(MIRROR_HOLE)).take(pads));
        }
        self.mirror.push(0.0);
        let n = self.mirror.len() - 1;
        self.mirror_sync(n, slot, false); // freshly appended: never a pre-existing hole
    }

    /// One-load dense element read: `Some(f)` is the element's Num value; `None` means the
    /// mirror can't answer (off, out of range, or a hole) — fall back to the classic path,
    /// which is always correct.
    #[inline]
    pub(crate) fn mirror_get(&self, n: u32) -> Option<f64> {
        if self.mirror_flags & MIRROR_OK == 0 {
            return None;
        }
        let f = *self.mirror.get(n as usize)?;
        if f.to_bits() == MIRROR_HOLE {
            return None;
        }
        Some(f)
    }

    /// Overwrite dense element `n`'s value keeping the mirror coherent. `Err` hands the value
    /// back: no such element, or it isn't a plain writable data property — the caller runs the
    /// generic path.
    #[inline]
    pub(crate) fn set_index_value(&mut self, n: u32, v: Value) -> Result<(), Value> {
        let Some(&slot) = self.elems.get(n as usize) else {
            return Err(v);
        };
        if slot == NO_SLOT {
            return Err(v);
        }
        let p = &mut self.entries[slot as usize].1;
        if p.accessor || !p.writable {
            return Err(v);
        }
        if self.mirror_flags & MIRROR_OK != 0 {
            match &v {
                Value::Num(f) if f.to_bits() != MIRROR_HOLE => {
                    if !f64_exact_i32(*f) {
                        self.mirror_flags &= !MIRROR_ALL_I32;
                    }
                    // Lockstep holds whenever the flag does; guard anyway.
                    match self.mirror.get_mut(n as usize) {
                        Some(m) => *m = *f,
                        None => self.mirror_invalidate(),
                    }
                }
                _ => self.mirror_invalidate(),
            }
        }
        let p = &mut self.entries[slot as usize].1;
        p.value = v;
        Ok(())
    }

    /// Record a fresh entry at `slot` in the dense map when its key is a canonical index at (or
    /// within a small pad of) the dense frontier. Far-past-the-frontier keys stay map-only.
    fn note_inserted(&mut self, slot: usize) {
        if slot >= NO_SLOT as usize {
            return;
        }
        let key = &self.entries[slot].0;
        if !key.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            return;
        }
        if let Some(n) = canonical_index(key) {
            let n = n as usize;
            if n < self.elems.len() {
                let filled_hole = self.elems[n] == NO_SLOT;
                self.elems[n] = slot as u32;
                self.mirror_sync(n, slot, filled_hole);
            } else if n <= self.elems.len() + 256 {
                // The pad tolerates *descending* first-fills (`while (--i >= 0) a[i] = 0`,
                // `r[i+n] = x[i]` from the top — bignum/matrix code does this constantly): the
                // first write lands well past the frontier, and a too-small pad would leave the
                // whole upper range map-only for the array's lifetime, killing every dense fast
                // path. 256 covers real dense workloads; a truly sparse `a[1e6]` still stays
                // map-only at ≤1KB of hole slots per object.
                let pads = n - self.elems.len();
                while self.elems.len() < n {
                    self.elems.push(NO_SLOT);
                }
                self.elems.push(slot as u32);
                self.mirror_grow(pads, slot);
            } else {
                self.has_far.set(true);
            }
        }
    }

    /// Dense tail append: insert element `n` when `n` is exactly the dense frontier and no
    /// map-only ("far") canonical key exists — which together prove the key is absent, so the
    /// whole existence scan and key-string hashing of [`Props::insert`] can be skipped. Array
    /// (`elem_mode`) maps only: the shape is untouched. Returns `false` (nothing changed) when
    /// the gates don't hold; the caller runs the generic path.
    pub(crate) fn append_element(&mut self, n: u32, prop: Property) -> bool {
        if self.has_far.get() || !self.elem_mode.get() || n as usize != self.elems.len() {
            return false;
        }
        self.note_structural();
        let slot = self.entries.len();
        let key = index_key(n as usize);
        if !self.index.is_empty() {
            self.index.insert(key.clone(), slot);
        } else if slot + 1 > INDEX_THRESHOLD {
            self.build_index();
            self.index.insert(key.clone(), slot);
        }
        self.entries.push((key, prop));
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
        true
    }

    /// Dense tail pop: remove element `n` (the array's last) when it is also the last *entry*
    /// (the common stack discipline — elements are appended last) and the last dense slot, and
    /// no "far" canonical key exists. Everything is O(1) pops: no entry shift, no re-index, no
    /// shape change (`elem_mode` maps keep their shape — element keys aren't part of it).
    /// `Some(value)` = removed; `None` = gates failed, nothing changed, caller goes generic.
    pub(crate) fn pop_last_element(&mut self, n: u32) -> Option<Value> {
        if self.has_far.get() || !self.elem_mode.get() {
            return None;
        }
        if n as usize + 1 != self.elems.len() {
            return None;
        }
        let slot = self.elems[n as usize];
        if slot == NO_SLOT || slot as usize + 1 != self.entries.len() {
            return None;
        }
        let p = &self.entries[slot as usize].1;
        if p.accessor || !p.configurable {
            return None;
        }
        self.note_structural();
        let (_, p) = self.entries.pop().unwrap();
        self.elems.pop();
        if !self.index.is_empty() {
            self.index.remove(&index_key(n as usize));
        }
        if self.mirror_flags & MIRROR_OK != 0 {
            debug_assert_eq!(self.mirror.len(), self.elems.len() + 1);
            let m = self.mirror.pop();
            if m.map(f64::to_bits) == Some(MIRROR_HOLE) {
                // (Unreachable while the slot was live, but keep the accounting exact.)
                self.mirror_holes -= 1;
                if self.mirror_holes == 0 {
                    self.mirror_flags |= MIRROR_NO_HOLES;
                }
            }
        }
        Some(p.value)
    }
    /// The entry slot for `key`. Small maps (≤ [`INDEX_THRESHOLD`] entries — most objects) have
    /// no hash index at all: lookup is a short linear scan and inserts never hash or rehash.
    /// The index is built once when a map grows past the threshold and is authoritative from
    /// then on (an emptied-but-once-large map keeps using it).
    #[inline]
    fn find(&self, key: &str) -> Option<usize> {
        // `length` and `prototype` are the hottest keys in array-heavy / allocation-heavy code
        // (every push/pop/length read; every `new`); their slots are memoized — answer without
        // hashing or scanning.
        if key == "length" {
            let s = self.len_slot.get();
            if s != NO_SLOT {
                debug_assert!(
                    matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "length")
                );
                return Some(s as usize);
            }
        } else if key == "prototype" {
            let s = self.proto_slot.get();
            if s != NO_SLOT {
                debug_assert!(
                    matches!(self.entries.get(s as usize), Some((k, _)) if &**k == "prototype")
                );
                return Some(s as usize);
            }
        }
        let found = if self.index.is_empty() {
            self.entries.iter().position(|(k, _)| &**k == key)
        } else {
            self.index.get(key).copied()
        };
        if let Some(s) = found {
            if key == "length" {
                self.len_slot.set(s as u32);
            } else if key == "prototype" {
                self.proto_slot.set(s as u32);
            }
        }
        found
    }
    /// Build the hash index for every current entry (crossing the small-map threshold).
    fn build_index(&mut self) {
        for (j, (k, _)) in self.entries.iter().enumerate() {
            self.index.insert(k.clone(), j);
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&Property> {
        self.find(key).map(|i| &self.entries[i].1)
    }
    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut Property> {
        if key.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        match self.find(key) {
            Some(i) => Some(&mut self.entries[i].1),
            None => None,
        }
    }
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.find(key).is_some()
    }
    /// The `entries` slot for `key`, or `None`. Backs the bytecode property inline cache: a hit
    /// records the slot so the next access can skip the lookup (see `Interp::try_ic_get`).
    #[inline]
    pub(crate) fn slot_of(&self, key: &str) -> Option<usize> {
        self.find(key)
    }
    /// The (key, property) at `slot`, or `None` if out of range. The caller re-checks the key —
    /// slots shift on `remove`, so a cached slot is only trusted after the key matches.
    #[inline]
    pub(crate) fn entry_at(&self, slot: usize) -> Option<&(Rc<str>, Property)> {
        self.entries.get(slot)
    }
    /// Mutable [`entry_at`], for the property write inline cache.
    #[inline]
    pub(crate) fn entry_at_mut(&mut self, slot: usize) -> Option<&mut (Rc<str>, Property)> {
        if self
            .entries
            .get(slot)
            .is_some_and(|(k, _)| k.as_bytes().first().is_some_and(|b| b.is_ascii_digit()))
        {
            self.mirror_invalidate(); // could be an element (see `mirror`)
        }
        self.entries.get_mut(slot)
    }
    /// Drop every property (used by the GC to break a garbage object's reference cycles).
    pub(crate) fn clear(&mut self) {
        self.note_structural();
        self.entries.clear();
        self.index.clear();
        self.elems.clear();
        self.mirror.clear();
        self.mirror_flags = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;
        self.mirror_holes = 0;
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        self.shape = shape_fresh();
    }
    /// Append the next dense element while *building a fresh array in order* (element index ==
    /// entry slot == dense slot): skips the canonical-index parse and, for small indices, the
    /// key-string allocation. Only valid on a Props whose entries so far are exactly the dense
    /// elements 0..len.
    pub(crate) fn push_dense(&mut self, prop: Property) {
        let slot = self.entries.len();
        let key = index_key(slot);
        if !self.index.is_empty() {
            self.index.insert(key.clone(), slot);
        } else if slot + 1 > INDEX_THRESHOLD {
            self.build_index();
            self.index.insert(key.clone(), slot);
        }
        self.entries.push((key, prop));
        self.elems.push(slot as u32);
        self.mirror_grow(0, slot);
    }

    /// Insert a key *known to be absent* (the caller shape-validated the map), landing on a
    /// *known* child shape: skips both the existence scan and the transition-table lookup that
    /// [`Props::insert`] pays. `new_shape` must be the memoized `shape_transition(shape, key)`
    /// result recorded when this (shape, key) pair was first inserted the slow way.
    pub(crate) fn append_new(&mut self, key: Rc<str>, prop: Property, new_shape: u32) {
        self.note_structural();
        let slot = self.entries.len();
        if !self.index.is_empty() {
            self.index.insert(key.clone(), slot);
        } else if slot + 1 > INDEX_THRESHOLD {
            self.build_index();
            self.index.insert(key.clone(), slot);
        }
        self.shape = new_shape;
        self.entries.push((key, prop));
        self.note_inserted(slot);
    }

    pub(crate) fn insert(&mut self, key: impl Into<Rc<str>>, prop: Property) {
        let key = key.into();
        if let Some(i) = self.find(&key) {
            self.entries[i].1 = prop;
            if self.mirror_flags & MIRROR_OK != 0
                && key.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
            {
                match canonical_index(&key) {
                    Some(n) if (n as usize) < self.mirror.len() => {
                        // Replacing an existing entry: position n already had the element.
                        self.mirror_sync(n as usize, i, false)
                    }
                    // A far/map-only index entry stays outside the mirror's range: fine.
                    Some(_) => {}
                    None => self.mirror_invalidate(), // "007"-style: not canonical, be safe
                }
            }
        } else {
            self.note_structural();
            let slot = self.entries.len();
            if !self.index.is_empty() {
                self.index.insert(key.clone(), slot);
            } else if slot + 1 > INDEX_THRESHOLD {
                self.build_index();
                self.index.insert(key.clone(), slot);
            }
            // Extending the key sequence transitions to the (shared, memoized) child shape.
            // Array element keys don't (see `elem_mode`): array shapes track named keys only.
            if !(self.elem_mode.get() && canonical_index(&key).is_some()) {
                self.shape = shape_transition(self.shape, &key);
            }
            if &*key == "length" {
                self.len_slot.set(slot as u32);
            } else if &*key == "prototype" {
                self.proto_slot.set(slot as u32);
            }
            self.entries.push((key, prop));
            self.note_inserted(slot);
        }
    }
    /// Remove every canonical-index key `>= from` in one pass — array truncation
    /// (`arr.length = n`). Entries compact and the lookup/dense maps rebuild once: O(n) total,
    /// where the per-key [`Props::remove`] loop it replaces was O(n) *per key*.
    pub(crate) fn remove_indices_from(&mut self, from: usize) {
        let keep = |k: &str| match canonical_index(k) {
            Some(n) => (n as usize) < from,
            None => true,
        };
        if self.entries.iter().all(|(k, _)| keep(k)) {
            return;
        }
        self.note_structural();
        self.entries.retain(|(k, _)| keep(k));
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        self.index.clear();
        if self.entries.len() > INDEX_THRESHOLD {
            self.build_index();
        }
        self.elems.clear();
        self.mirror.clear();
        self.mirror_flags = MIRROR_OK | MIRROR_ALL_I32 | MIRROR_NO_HOLES;
        self.mirror_holes = 0;
        for slot in 0..self.entries.len() {
            self.note_inserted(slot);
        }
        // A removal shifts slots: it can't be a tree transition, so deopt to a fresh unique id.
        self.shape = shape_fresh();
    }

    pub(crate) fn remove(&mut self, key: &str) -> bool {
        let Some(i) = self.find(key) else {
            return false;
        };
        self.note_structural();
        self.entries.remove(i);
        // Slots shifted — deopt to a fresh shape id (see remove_indices_from). Array maps skip
        // this for ELEMENT keys: their shape tracks named keys only, and array entry slots are
        // only ever trusted through key-checked ICs (IC_ARR_KEYCHK), which re-verify on hit.
        if !(self.elem_mode.get() && canonical_index(key).is_some()) {
            self.shape = shape_fresh();
        }
        self.len_slot.set(NO_SLOT);
        self.proto_slot.set(NO_SLOT);
        if !self.index.is_empty() {
            self.index.remove(key);
            // Re-index everything after the removed slot.
            for (j, (k, _)) in self.entries.iter().enumerate().skip(i) {
                self.index.insert(k.clone(), j);
            }
        }
        if self.mirror_flags & MIRROR_OK != 0 {
            match canonical_index(key) {
                Some(n) if (n as usize) < self.mirror.len() => {
                    if self.mirror[n as usize].to_bits() != MIRROR_HOLE {
                        self.mirror[n as usize] = f64::from_bits(MIRROR_HOLE);
                        self.mirror_flags &= !MIRROR_NO_HOLES;
                        self.mirror_holes += 1;
                    }
                }
                Some(_) | None => {}
            }
        }
        // Dense slots shift down past the removed entry; the removed key's own slot holes.
        for e in self.elems.iter_mut() {
            if *e == NO_SLOT {
                continue;
            }
            match (*e as usize).cmp(&i) {
                std::cmp::Ordering::Equal => *e = NO_SLOT,
                std::cmp::Ordering::Greater => *e -= 1,
                std::cmp::Ordering::Less => {}
            }
        }
        true
    }
    /// Keys in insertion order. Private-name slots (`#x`) are never enumerable/observable, so they
    /// are excluded here (and from [`ordered_keys`]); private access reads them via [`get`] directly.
    pub(crate) fn keys(&self) -> Vec<Rc<str>> {
        self.entries
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !crate::interpreter::Interp::is_private_key(k))
            .collect()
    }
    /// Keys in spec [[OwnPropertyKeys]] order: array-index keys ascending, then other string keys
    /// in insertion order, then symbol keys in insertion order.
    pub(crate) fn ordered_keys(&self) -> Vec<Rc<str>> {
        let mut ints: Vec<(u32, Rc<str>)> = Vec::new();
        let mut strs: Vec<Rc<str>> = Vec::new();
        let mut syms: Vec<Rc<str>> = Vec::new();
        for (k, _) in &self.entries {
            if crate::interpreter::Interp::is_private_key(k) {
                continue; // private-element slot — not an observable own key
            }
            if crate::interpreter::Interp::is_sym_key(k) {
                syms.push(k.clone());
            } else if let Some(n) = canonical_index(k) {
                ints.push((n, k.clone()));
            } else {
                strs.push(k.clone());
            }
        }
        ints.sort_by_key(|(n, _)| *n);
        ints.into_iter()
            .map(|(_, k)| k)
            .chain(strs)
            .chain(syms)
            .collect()
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Property)> {
        self.entries.iter().map(|(k, p)| (k, p))
    }
}

/// A canonical array-index property key (`"0"`, `"42"` — decimal, no leading zeros, fits u32).
pub(crate) fn canonical_index(k: &str) -> Option<u32> {
    if k == "0" {
        return Some(0);
    }
    if k.is_empty() || k.starts_with('0') || !k.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    k.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

/// Convenience: define a plain own data property by key/value.
pub fn set_data(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::plain(value));
}

/// Convenience: define a non-enumerable builtin property by key/value.
pub fn set_builtin(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::builtin(value));
}

/// IEEE-754 half-precision (binary16) to single-precision conversion.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: normalize into a single-precision normal number.
            let mut e: i32 = -1;
            let mut m = mant;
            loop {
                e += 1;
                m <<= 1;
                if m & 0x400 != 0 {
                    break;
                }
            }
            let m = m & 0x3ff;
            sign | (((127 - 15 - e) as u32) << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | (((exp as u32) + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// IEEE-754 double-precision to half-precision (binary16), round-to-nearest-even, rounding **once**.
/// Going through `f32` first would double-round — e.g. `2^-25 + ε` collapses to an exact tie at
/// `f32` and then rounds to zero instead of up to the smallest subnormal.
pub fn f64_to_f16(value: f64) -> u16 {
    let x = value.to_bits();
    let sign = ((x >> 48) & 0x8000) as u16;
    let exp = ((x >> 52) & 0x7ff) as i32;
    let mant = x & 0x000f_ffff_ffff_ffff; // 52-bit fraction
    if exp == 0x7ff {
        return if mant != 0 {
            sign | 0x7e00 // NaN
        } else {
            sign | 0x7c00 // infinity
        };
    }
    if exp == 0 && mant == 0 {
        return sign; // signed zero
    }
    let half_exp = exp - 1023 + 15;
    if half_exp >= 0x1f {
        return sign | 0x7c00; // overflow → infinity
    }
    if half_exp <= 0 {
        // Subnormal half (or underflow to zero). Drop the low bits of the full significand,
        // rounding to nearest even. `exp == 0` doubles are far below f16 range → they fall out as 0.
        let m = if exp == 0 { mant } else { mant | (1u64 << 52) };
        let shift = 43 - half_exp; // 52-bit fraction → 10-bit fraction, minus the exponent deficit
        if shift >= 64 {
            return sign;
        }
        let mut h = (m >> shift) as u16;
        let round_bit = (m >> (shift - 1)) & 1;
        let sticky = (m & ((1u64 << (shift - 1)) - 1)) != 0;
        if round_bit != 0 && (sticky || (h & 1) != 0) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = (((half_exp as u32) << 10) | ((mant >> 42) as u32)) as u16;
    let round_bit = (mant >> 41) & 1;
    let sticky = (mant & ((1u64 << 41) - 1)) != 0;
    if round_bit != 0 && (sticky || (h & 1) != 0) {
        h = h.wrapping_add(1); // carry into exponent is intentional
    }
    sign | h
}
