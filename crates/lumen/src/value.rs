//! Runtime values and the object model. Objects are `Rc<RefCell<Object>>` ([`Gc`]); there is no
//! real garbage collector yet (reference counting, so cycles leak — acceptable for the test262
//! loop). Properties are stored in insertion order in a small map.

use crate::ast::Function;
use crate::interpreter::{Env, Interp};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

pub type Gc = Rc<RefCell<Object>>;

/// A native (Rust-implemented) function. It can only throw (via `Err`), never break/return/continue,
/// so a plain `Result<Value, Value>` (Err = the thrown value) is the whole contract.
pub type NativeFn = fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    /// BigInt, approximated with `i128` (exact within ±2^127; tests beyond that range fail rather
    /// than implementing arbitrary precision).
    BigInt(i128),
    Str(Rc<str>),
    Sym(Rc<SymbolData>),
    Obj(Gc),
}

/// A unique Symbol. Identity is the `id` (every `Symbol()` call gets a fresh one); `description` is
/// the optional label. Well-known symbols (`Symbol.iterator`, …) are just pre-allocated instances.
pub struct SymbolData {
    pub id: u64,
    pub description: Option<Rc<str>>,
}

impl Value {
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }
    pub fn from_string(s: String) -> Value {
        Value::Str(Rc::from(s.as_str()))
    }
    pub fn as_obj(&self) -> Option<&Gc> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Obj(o) if !matches!(o.borrow().call, Callable::None))
    }
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
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
    StrWrap(Rc<str>),
    /// An error object — carries no extra data (name/message live as ordinary properties) but the
    /// tag lets `Error.prototype.toString` and the test262 runner recognise it cheaply.
    Error,
}

pub struct Object {
    pub proto: Option<Gc>,
    pub props: Props,
    pub extensible: bool,
    pub call: Callable,
    pub exotic: Exotic,
    /// The construct-time prototype handed to instances (`F.prototype`), cached for `new`.
    pub is_constructor: bool,
    /// GC scratch: mark bit (reachability) and a count of references from other heap objects.
    pub gc_mark: Cell<bool>,
    pub gc_internal: Cell<u32>,
}

impl Object {
    pub fn new(proto: Option<Gc>) -> Gc {
        LIVE_OBJECTS.with(|c| c.set(c.get() + 1));
        let obj = Rc::new(RefCell::new(Object {
            proto,
            props: Props::new(),
            extensible: true,
            call: Callable::None,
            exotic: Exotic::None,
            is_constructor: false,
            gc_mark: Cell::new(false),
            gc_internal: Cell::new(0),
        }));
        GC_REGISTRY.with(|r| r.borrow_mut().push(Rc::downgrade(&obj)));
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
thread_local! {
    static GC_REGISTRY: RefCell<Vec<Weak<RefCell<Object>>>> = const { RefCell::new(Vec::new()) };
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
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|w| match w.upgrade() {
            Some(o) => {
                live.push(o);
                true
            }
            None => false,
        });
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
    pub fn elsize(self) -> usize {
        match self {
            TaKind::I8 | TaKind::U8 | TaKind::U8Clamped => 1,
            TaKind::I16 | TaKind::U16 | TaKind::F16 => 2,
            TaKind::I32 | TaKind::U32 | TaKind::F32 => 4,
            TaKind::F64 | TaKind::I64 | TaKind::U64 => 8,
        }
    }
    /// Whether elements are BigInt (BigInt64Array / BigUint64Array) rather than Number.
    pub fn is_bigint(self) -> bool {
        matches!(self, TaKind::I64 | TaKind::U64)
    }
    /// Constructor / prototype name, e.g. "Int8Array".
    pub fn name(self) -> &'static str {
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
    pub fn read_bigint(self, b: &[u8]) -> i128 {
        let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match self {
            TaKind::U64 => u64::from_le_bytes(arr) as i128,
            _ => i64::from_le_bytes(arr) as i128,
        }
    }
    /// Convert a BigInt (i128) to this element's 8 little-endian bytes, wrapping mod 2^64.
    pub fn write_bigint(self, n: i128) -> Vec<u8> {
        (n as u64).to_le_bytes().to_vec()
    }
    /// Read one element (little-endian) from `b` (which must be `elsize()` bytes) as a Number.
    pub fn read(self, b: &[u8]) -> f64 {
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
    pub fn write(self, n: f64) -> Vec<u8> {
        let int = |n: f64| if n.is_finite() { n.trunc() as i64 } else { 0 };
        match self {
            TaKind::I8 => vec![int(n) as i8 as u8],
            TaKind::U8 => vec![int(n) as u8],
            TaKind::U8Clamped => {
                let c = if n.is_nan() {
                    0.0
                } else {
                    n.round().clamp(0.0, 255.0)
                };
                vec![c as u8]
            }
            TaKind::I16 => (int(n) as i16).to_le_bytes().to_vec(),
            TaKind::U16 => (int(n) as u16).to_le_bytes().to_vec(),
            TaKind::I32 => (int(n) as i32).to_le_bytes().to_vec(),
            TaKind::U32 => (int(n) as u32).to_le_bytes().to_vec(),
            TaKind::F16 => f32_to_f16(n as f32).to_le_bytes().to_vec(),
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

/// A property descriptor. A data property uses `value`/`writable`; an accessor uses `get`/`set`.
#[derive(Clone)]
pub struct Property {
    pub value: Value,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub accessor: bool,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

impl Property {
    pub fn data(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Property {
        Property {
            value,
            get: None,
            set: None,
            accessor: false,
            writable,
            enumerable,
            configurable,
        }
    }
    /// A default plain data property: writable, enumerable, configurable.
    pub fn plain(value: Value) -> Property {
        Property::data(value, true, true, true)
    }
    /// A non-enumerable method/builtin property: writable + configurable, not enumerable.
    pub fn builtin(value: Value) -> Property {
        Property::data(value, true, false, true)
    }
}

/// Insertion-ordered string-keyed property map. A `Vec` of entries preserves order (good enough for
/// `for-in`/`Object.keys`); a side `HashMap` keeps lookup O(1).
pub struct Props {
    entries: Vec<(Rc<str>, Property)>,
    index: HashMap<Rc<str>, usize>,
}

impl Default for Props {
    fn default() -> Self {
        Self::new()
    }
}

impl Props {
    pub fn new() -> Props {
        Props {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }
    pub fn get(&self, key: &str) -> Option<&Property> {
        self.index.get(key).map(|i| &self.entries[*i].1)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Property> {
        if let Some(i) = self.index.get(key) {
            Some(&mut self.entries[*i].1)
        } else {
            None
        }
    }
    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
    /// Drop every property (used by the GC to break a garbage object's reference cycles).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }
    pub fn insert(&mut self, key: impl Into<Rc<str>>, prop: Property) {
        let key = key.into();
        if let Some(i) = self.index.get(&key) {
            self.entries[*i].1 = prop;
        } else {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push((key, prop));
        }
    }
    pub fn remove(&mut self, key: &str) -> bool {
        if let Some(i) = self.index.remove(key) {
            self.entries.remove(i);
            // Re-index everything after the removed slot.
            for (j, (k, _)) in self.entries.iter().enumerate().skip(i) {
                self.index.insert(k.clone(), j);
            }
            true
        } else {
            false
        }
    }
    /// Drop every entry whose key fails `keep`, rebuilding the index once (O(n) total). Use this for
    /// bulk removal — repeated [`remove`] is O(n) each, so deleting many keys that way is O(n²).
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.entries.retain(|(k, _)| keep(k));
        self.index.clear();
        for (i, (k, _)) in self.entries.iter().enumerate() {
            self.index.insert(k.clone(), i);
        }
    }
    /// Keys in insertion order. Private-name slots (`#x`) are never enumerable/observable, so they
    /// are excluded here (and from [`ordered_keys`]); private access reads them via [`get`] directly.
    pub fn keys(&self) -> Vec<Rc<str>> {
        self.entries
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !k.starts_with('#'))
            .collect()
    }
    /// Keys in spec [[OwnPropertyKeys]] order: array-index keys ascending, then other string keys
    /// in insertion order, then symbol keys in insertion order.
    pub fn ordered_keys(&self) -> Vec<Rc<str>> {
        let mut ints: Vec<(u32, Rc<str>)> = Vec::new();
        let mut strs: Vec<Rc<str>> = Vec::new();
        let mut syms: Vec<Rc<str>> = Vec::new();
        for (k, _) in &self.entries {
            if k.starts_with('#') {
                continue; // private-name slot — not an observable own key
            }
            if k.starts_with('\u{0}') {
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
    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Property)> {
        self.entries.iter().map(|(k, p)| (k, p))
    }
}

/// A canonical array-index property key (`"0"`, `"42"` — decimal, no leading zeros, fits u32).
fn canonical_index(k: &str) -> Option<u32> {
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

/// IEEE-754 single-precision to half-precision (binary16), round-to-nearest-even.
pub fn f32_to_f16(value: f32) -> u16 {
    let x = value.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let mant = (x & 0x7f_ffff) as i32;
    let exp = ((x >> 23) & 0xff) as i32;
    if exp == 0xff {
        return if mant != 0 {
            sign | 0x7e00
        } else {
            sign | 0x7c00
        };
    }
    let half_exp = exp - 127 + 15;
    if half_exp >= 0x1f {
        return sign | 0x7c00; // overflow → infinity
    }
    if half_exp <= 0 {
        if half_exp < -10 {
            return sign; // underflow → zero
        }
        // Subnormal: shift the implicit-1 mantissa, rounding to nearest even.
        let m = mant | 0x80_0000;
        let shift = 14 - half_exp;
        let mut h = (m >> shift) as u16;
        let round_bit = (m >> (shift - 1)) & 1;
        let sticky = (m & ((1 << (shift - 1)) - 1)) != 0;
        if round_bit != 0 && (sticky || (h & 1) != 0) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = (((half_exp as u32) << 10) | ((mant >> 13) as u32)) as u16;
    let round_bit = (mant >> 12) & 1;
    let sticky = (mant & 0xfff) != 0;
    if round_bit != 0 && (sticky || (h & 1) != 0) {
        h = h.wrapping_add(1); // carry into exponent is intentional
    }
    sign | h
}
