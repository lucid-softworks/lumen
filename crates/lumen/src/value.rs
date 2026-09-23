//! Runtime values and the object model. Objects are `Rc<RefCell<Object>>` ([`Gc`]); there is no
//! real garbage collector yet (reference counting, so cycles leak — acceptable for the test262
//! loop). Properties are stored in insertion order in a small map.

use crate::ast::Function;
use crate::interpreter::{Env, Interp};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

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

// NaN-boxed storage used for long-lived property values. Execution still uses the ergonomic
// `Value` enum while the migration is staged; packing at the heap boundary cuts each ordinary
// property by eight bytes without coupling the experiment to every interpreter pattern match.
#[repr(transparent)]
pub(crate) struct PackedValue(u64);

const PACK_PAYLOAD: u64 = 0x0000_ffff_ffff_ffff;
pub(crate) const PACK_UNDEFINED: u64 = 0x7ff9_0000_0000_0000;
pub(crate) const PACK_EMPTY: u64 = 0x7ffa_0000_0000_0000;
pub(crate) const PACK_NULL: u64 = 0x7ffb_0000_0000_0000;
pub(crate) const PACK_BOOL: u64 = 0x7ffc_0000_0000_0000;
pub(crate) const PACK_BIGINT: u64 = 0x7ffd_0000_0000_0000;
pub(crate) const PACK_STR: u64 = 0x7ffe_0000_0000_0000;
pub(crate) const PACK_SYM: u64 = 0x7fff_0000_0000_0000;
pub(crate) const PACK_OBJ: u64 = 0xfff9_0000_0000_0000;
pub(crate) const PACK_CANON_NAN: u64 = 0x7ff8_0000_0000_0000;

impl PackedValue {
    #[inline]
    fn tag(&self) -> u64 {
        self.0 & !PACK_PAYLOAD
    }

    unsafe fn into_word<T>(value: T) -> u64 {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let value = std::mem::ManuallyDrop::new(value);
        let mut word = 0usize;
        unsafe {
            std::ptr::copy_nonoverlapping(
                &*value as *const T as *const u8,
                &mut word as *mut usize as *mut u8,
                std::mem::size_of::<T>(),
            );
        }
        let word = word as u64;
        assert_eq!(
            word & !PACK_PAYLOAD,
            0,
            "pointer does not fit NaN-box payload"
        );
        word
    }

    unsafe fn read_word<T>(&self) -> T {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let word = (self.0 & PACK_PAYLOAD) as usize;
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &word as *const usize as *const u8,
                value.as_mut_ptr() as *mut u8,
                std::mem::size_of::<T>(),
            );
            value.assume_init()
        }
    }

    unsafe fn clone_word<T: Clone>(&self) -> T {
        let value = std::mem::ManuallyDrop::new(unsafe { self.read_word::<T>() });
        T::clone(&value)
    }

    unsafe fn drop_word<T>(&mut self) {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let word = (self.0 & PACK_PAYLOAD) as usize;
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &word as *const usize as *const u8,
                value.as_mut_ptr() as *mut u8,
                std::mem::size_of::<T>(),
            );
            value.assume_init_drop();
        }
    }

    pub(crate) fn pack(value: Value) -> PackedValue {
        let bits = match value {
            Value::Undefined => PACK_UNDEFINED,
            Value::Empty => PACK_EMPTY,
            Value::Null => PACK_NULL,
            Value::Bool(v) => PACK_BOOL | v as u64,
            Value::Num(v) => {
                if v.is_nan() {
                    PACK_CANON_NAN
                } else {
                    v.to_bits()
                }
            }
            Value::BigInt(v) => PACK_BIGINT | unsafe { Self::into_word(v) },
            Value::Str(v) => PACK_STR | unsafe { Self::into_word(v) },
            Value::Sym(v) => PACK_SYM | unsafe { Self::into_word(v) },
            Value::Obj(v) => PACK_OBJ | unsafe { Self::into_word(v) },
        };
        PackedValue(bits)
    }

    pub(crate) fn unpack(&self) -> Value {
        match self.tag() {
            PACK_UNDEFINED => Value::Undefined,
            PACK_EMPTY => Value::Empty,
            PACK_NULL => Value::Null,
            PACK_BOOL => Value::Bool(self.0 & 1 != 0),
            PACK_BIGINT => Value::BigInt(unsafe { self.clone_word() }),
            PACK_STR => Value::Str(unsafe { self.clone_word() }),
            PACK_SYM => Value::Sym(unsafe { self.clone_word() }),
            PACK_OBJ => Value::Obj(unsafe { self.clone_word() }),
            _ => Value::Num(f64::from_bits(self.0)),
        }
    }

    /// Consume the packed owner without a refcount round trip. Pointer payload bits become the
    /// returned `Value`'s ownership; `ManuallyDrop` prevents this container from releasing them.
    pub(crate) fn into_value(self) -> Value {
        let this = std::mem::ManuallyDrop::new(self);
        match this.tag() {
            PACK_UNDEFINED => Value::Undefined,
            PACK_EMPTY => Value::Empty,
            PACK_NULL => Value::Null,
            PACK_BOOL => Value::Bool(this.0 & 1 != 0),
            PACK_BIGINT => Value::BigInt(unsafe { this.read_word() }),
            PACK_STR => Value::Str(unsafe { this.read_word() }),
            PACK_SYM => Value::Sym(unsafe { this.read_word() }),
            PACK_OBJ => Value::Obj(unsafe { this.read_word() }),
            _ => Value::Num(f64::from_bits(this.0)),
        }
    }

    /// Drop one owned packed word in raw frame storage without first widening the frame.
    pub(crate) unsafe fn drop_raw(word: *mut u64) {
        drop(unsafe { std::ptr::read(word as *const PackedValue) });
    }

    /// Clone one packed owner out of raw frame storage into a wide execution value.
    pub(crate) unsafe fn clone_raw(word: *const u64) -> Value {
        unsafe { &*(word as *const PackedValue) }.unpack()
    }

    /// Replace one packed owner with a moved wide value and drop the previous owner.
    pub(crate) unsafe fn replace_raw(word: *mut u64, value: Value) {
        let old = unsafe { std::ptr::replace(word as *mut PackedValue, PackedValue::pack(value)) };
        drop(old);
    }

    /// Compact `len` initialized wide values into the first half of the same allocation. The
    /// source stride is 16 and destination stride is 8, so a forward walk never overwrites a
    /// source that has not been moved yet.
    ///
    /// # Safety
    /// `base` must address `len` initialized contiguous `Value`s and enough aligned storage for
    /// them. After return only `len` packed words at `base` are initialized.
    pub(crate) unsafe fn pack_in_place(base: *mut Value, len: usize) {
        let packed = base.cast::<PackedValue>();
        for k in 0..len {
            let value = unsafe { base.add(k).read() };
            unsafe { packed.add(k).write(PackedValue::pack(value)) };
        }
    }

    /// Expand packed frame words back into wide `Value`s without cloning reference payloads.
    /// Expansion walks backward so each packed source is consumed before a wider destination can
    /// overlap it.
    ///
    /// # Safety
    /// `base` must address `len` initialized `PackedValue`s followed by enough aligned storage for
    /// `len` wide values. After return only those wide values are initialized.
    pub(crate) unsafe fn unpack_in_place(base: *mut Value, len: usize) {
        let packed = base.cast::<PackedValue>();
        for k in (0..len).rev() {
            let value = unsafe { packed.add(k).read() }.into_value();
            unsafe { base.add(k).write(value) };
        }
    }
}

impl Clone for PackedValue {
    fn clone(&self) -> Self {
        PackedValue::pack(self.unpack())
    }
}

impl Drop for PackedValue {
    fn drop(&mut self) {
        match self.tag() {
            PACK_BIGINT => unsafe { self.drop_word::<crate::bigint::JsBigInt>() },
            PACK_STR => unsafe { self.drop_word::<crate::lstr::LStr>() },
            PACK_SYM => unsafe { self.drop_word::<Rc<SymbolData>>() },
            PACK_OBJ => unsafe { self.drop_word::<Gc>() },
            _ => {}
        }
    }
}

#[cfg(test)]
mod packed_value_tests {
    use super::*;

    #[test]
    fn packed_value_is_one_word_and_round_trips_scalars() {
        assert_eq!(std::mem::size_of::<PackedValue>(), 8);
        assert!(matches!(
            PackedValue::pack(Value::Undefined).into_value(),
            Value::Undefined
        ));
        assert!(matches!(
            PackedValue::pack(Value::Empty).into_value(),
            Value::Empty
        ));
        assert!(matches!(
            PackedValue::pack(Value::Null).into_value(),
            Value::Null
        ));
        assert!(matches!(
            PackedValue::pack(Value::Bool(false)).into_value(),
            Value::Bool(false)
        ));
        assert!(matches!(
            PackedValue::pack(Value::Bool(true)).into_value(),
            Value::Bool(true)
        ));
        for n in [0.0f64, -0.0, 42.5, f64::INFINITY, f64::NAN] {
            let Value::Num(out) = PackedValue::pack(Value::Num(n)).into_value() else {
                panic!("number changed kind")
            };
            assert!(n.is_nan() && out.is_nan() || n.to_bits() == out.to_bits());
        }
    }

    #[test]
    fn packed_value_moves_reference_ownership_without_a_bump() {
        let obj = Object::new(None);
        let before = Rc::strong_count(&obj);
        let packed = PackedValue::pack(Value::Obj(obj.clone()));
        assert_eq!(Rc::strong_count(&obj), before + 1);
        let out = packed.into_value();
        assert_eq!(Rc::strong_count(&obj), before + 1);
        drop(out);
        assert_eq!(Rc::strong_count(&obj), before);
    }

    #[test]
    fn packed_frame_conversion_is_overlap_safe_and_ownership_neutral() {
        let obj = Object::new(None);
        let before = Rc::strong_count(&obj);
        let mut frame: [std::mem::MaybeUninit<Value>; 5] =
            std::array::from_fn(|_| std::mem::MaybeUninit::uninit());
        let base = frame.as_mut_ptr().cast::<Value>();
        unsafe {
            base.add(0).write(Value::Num(1.5));
            base.add(1).write(Value::Obj(obj.clone()));
            base.add(2).write(Value::Bool(true));
            base.add(3).write(Value::Null);
            base.add(4).write(Value::Num(-0.0));
        }
        assert_eq!(Rc::strong_count(&obj), before + 1);
        unsafe {
            PackedValue::pack_in_place(base, 5);
            assert_eq!(Rc::strong_count(&obj), before + 1);
            PackedValue::unpack_in_place(base, 5);
            assert_eq!(Rc::strong_count(&obj), before + 1);
            assert!(matches!(&*base.add(0), Value::Num(n) if *n == 1.5));
            assert!(matches!(&*base.add(1), Value::Obj(o) if Rc::ptr_eq(o, &obj)));
            assert!(matches!(&*base.add(2), Value::Bool(true)));
            assert!(matches!(&*base.add(3), Value::Null));
            assert!(matches!(&*base.add(4), Value::Num(n) if n.to_bits() == (-0.0f64).to_bits()));
            for k in 0..5 {
                std::ptr::drop_in_place(base.add(k));
            }
        }
        assert_eq!(Rc::strong_count(&obj), before);
    }
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
    pub obj_is_constructor: usize,
    pub obj_extensible: usize,
    pub props_shape: usize,
    pub props_proto_flag: usize,
    /// The `entries` `Vec` within `Props`.
    pub props_entries: usize,
    /// The data-pointer word within a `Vec` (not necessarily offset 0 — RawVec layout is unstable).
    pub vec_ptr_off: usize,
    /// The length word within a `Vec` (probed like `vec_ptr_off`).
    pub vec_len_off: usize,
    /// The capacity word within a `Vec` (probed alongside pointer and length).
    pub vec_cap_off: usize,
    /// The nullable pointer to the shared boxed dense-buffer headers within `Props`.
    pub props_elems: usize,
    /// The `elems` Vec header within the shared dense-buffer allocation.
    pub dense_elems: usize,
    /// The `mirror` Vec header within the shared dense-buffer allocation.
    pub dense_mirror: usize,
    /// Nullable `Box<Vec<Property>>` within the dense sidecar. When non-null the box points at
    /// the Vec header; packed element slots use [`Value::Empty`] for holes and have no key Rc.
    pub dense_packed: usize,
    /// Initialized inline element count and slot base within DenseBuffers.
    pub dense_inline_len: usize,
    pub dense_inline_slots: usize,
    /// The `mirror_flags` byte within `Props`.
    pub props_mirror_flags: usize,
    /// `size_of::<(Rc<str>, Property)>()` — the entry stride.
    pub entry_size: usize,
    /// `Value` within an entry `(Rc<str>, Property)`.
    pub entry_value: usize,
    /// Descriptor flags byte within an entry (used to test `PROP_ACCESSOR`).
    pub entry_accessor: usize,
    /// Descriptor flags byte within an entry (used to test `PROP_WRITABLE`).
    pub entry_writable: usize,
    /// Standalone `Property` layout used by keyless packed elements (not tuple-entry offsets).
    pub property_size: usize,
    pub property_value: usize,
    pub property_meta: usize,
    /// The `Option<Box<Vec<Property>>>` niche and Vec header words matched the live probes.
    pub packed_elems_valid: bool,
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
    /// `value` within a `Binding` (the LoadName template's 16-byte copy source).
    pub binding_value: usize,
    /// `mutable` within a `Binding` (free-name update/store guard).
    pub binding_mutable: usize,
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
        let words: [usize; 2] = unsafe { std::mem::transmute_copy::<Rc<str>, [usize; 2]>(&probe) };
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
    let vec_cap_off = vwords.iter().position(|&w| w == 3).map(|i| i * 8);
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
        && vec_len_off.is_some_and(|o| v32words[o / 8] == 1)
        && vec_cap_off.is_some_and(|o| v32words[o / 8] == 3);

    // DenseStorage is deliberately a transparent nullable pointer to boxed buffer headers. The
    // JIT first follows this pointer and then uses the probed Vec offsets above. Verify the niche
    // than relying on it silently if a future compiler changes the representation.
    let thin_some = DenseStorage(Some(Box::new(DenseBuffers::default())));
    let thin_word = unsafe { *(&thin_some as *const DenseStorage as *const usize) };
    let thin_expected = thin_some.0.as_deref().unwrap() as *const DenseBuffers as usize;
    let thin_none = DenseStorage(None);
    let thin_none_word = unsafe { *(&thin_none as *const DenseStorage as *const usize) };
    let thin_vec_ok = thin_word == thin_expected && thin_none_word == 0;

    // Keyless packed-element sidecar: probe both Option<Box<_>>'s null niche and Vec<Property>'s
    // header words independently. The JIT follows the Box pointer, then uses the same located
    // Vec word offsets as the classic entry/element vectors.
    let mut pv = Vec::with_capacity(3);
    pv.push(Property::plain(Value::Num(1.0)));
    let pv_ptr = pv.as_ptr() as usize;
    let pv_words = unsafe {
        std::slice::from_raw_parts(
            &pv as *const Vec<Property> as *const usize,
            std::mem::size_of::<Vec<Property>>() / 8,
        )
    };
    let pv_header_ok = vec_ptr_off.is_some_and(|o| pv_words[o / 8] == pv_ptr)
        && vec_len_off.is_some_and(|o| pv_words[o / 8] == 1)
        && vec_cap_off.is_some_and(|o| pv_words[o / 8] == 3);
    let packed_some: Option<Box<Vec<Property>>> = Some(Box::new(pv));
    let packed_word =
        unsafe { *(&packed_some as *const Option<Box<Vec<Property>>> as *const usize) };
    let packed_expected = packed_some
        .as_deref()
        .map_or(0, |v| v as *const Vec<Property> as usize);
    let packed_none: Option<Box<Vec<Property>>> = None;
    let packed_none_word =
        unsafe { *(&packed_none as *const Option<Box<Vec<Property>>> as *const usize) };
    let packed_elems_valid =
        pv_header_ok && packed_word == packed_expected && packed_word != 0 && packed_none_word == 0;

    // Exotic::None / Exotic::Array discriminants (Exotic is repr(Rust); probe to be certain).
    let none = Exotic::None;
    let exotic_none_tag = unsafe { *(&none as *const Exotic as *const u8) };
    let arr = Exotic::Array;
    let exotic_array_tag = unsafe { *(&arr as *const Exotic as *const u8) };
    let sw = Exotic::str_wrap("".into());
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
    let binding_mutable = offset_of!(crate::interpreter::Binding, mutable);
    let binding_init = offset_of!(crate::interpreter::Binding, initialized);

    let valid = strong_ok
        && niche_ok
        && vec_ptr_off.is_some()
        && vec_len_off.is_some()
        && vec_cap_off.is_some()
        && vec32_ok
        && thin_vec_ok;
    JitLayout {
        obj_from_rc,
        gc_data_off,
        rc_strong_off,
        obj_proto: offset_of!(Object, proto),
        obj_ic_plain: offset_of!(Object, ic_plain),
        obj_props: offset_of!(Object, props),
        obj_exotic: offset_of!(Object, exotic),
        obj_is_constructor: offset_of!(Object, is_constructor),
        obj_extensible: offset_of!(Object, extensible),
        props_shape: offset_of!(Props, shape),
        props_proto_flag: offset_of!(Props, proto_flag),
        props_entries: offset_of!(Props, entries),
        vec_ptr_off: vec_ptr_off.unwrap_or(0),
        vec_len_off: vec_len_off.unwrap_or(0),
        vec_cap_off: vec_cap_off.unwrap_or(0),
        props_elems: offset_of!(Props, elems),
        dense_elems: offset_of!(DenseBuffers, elems),
        dense_mirror: offset_of!(DenseBuffers, mirror),
        dense_packed: offset_of!(DenseBuffers, packed),
        dense_inline_len: DenseBuffers::inline_len_offset(),
        dense_inline_slots: DenseBuffers::inline_slots_offset(),
        props_mirror_flags: offset_of!(Props, mirror_flags),
        entry_size: std::mem::size_of::<(Rc<str>, Property)>(),
        entry_key: offset_of!((Rc<str>, Property), 0),
        str_len_word,
        str_ptr_word,
        str_data_off,
        key_probe_ok,
        entry_value: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, packed),
        entry_accessor: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, meta),
        entry_writable: offset_of!((Rc<str>, Property), 1) + offset_of!(Property, meta),
        property_size: std::mem::size_of::<Property>(),
        property_value: offset_of!(Property, packed),
        property_meta: offset_of!(Property, meta),
        packed_elems_valid,
        exotic_none_tag,
        exotic_array_tag,
        exotic_strwrap_tag,
        scope_gen,
        binding_value,
        binding_mutable,
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
    NativeData(Rc<NativeCallable>),
    /// An interpreted function: its AST plus the lexical environment it closed over.
    User(Rc<UserCallable>),
    /// The result of `Function.prototype.bind`.
    Bound(Box<BoundCallable>),
    /// A ShadowRealm wrapped function: `target` is a callable inside the sub-realm identified by
    /// `realm` (its pointer). Calls marshal primitive args in and the primitive result out.
    WrappedShadow(Rc<WrappedShadowCallable>),
    /// The inverse: a function living *inside* a ShadowRealm whose `target` is a callable of the
    /// host realm. `realm` is this sub-realm's key in the host's map and `parent` is the host
    /// interpreter's stable address (hosts are either the engine root or boxed sub-realms, both
    /// pinned in memory while any of their sub-realm objects exist).
    WrappedCross(Box<WrappedCrossCallable>),
    /// An auto-accessor's synthesized getter: reads the private backing field (brand-checked) off
    /// the receiver.
    AccessorGet(Rc<Rc<str>>),
    /// An auto-accessor's synthesized setter: writes the private backing field (brand-checked).
    AccessorSet(Rc<Rc<str>>),
    /// A decorator `context.access.get`: returns `args[0][name]`.
    PropGet(Rc<Rc<str>>),
    /// A decorator `context.access.set`: performs `args[0][name] = args[1]`.
    PropSet(Rc<Rc<str>>),
}

/// Cold payloads boxed out of [`Callable`], so every non-callable ordinary object does not pay
/// for the largest function variants inline.
#[derive(Clone)]
pub struct BoundCallable {
    pub(crate) target: Gc,
    pub(crate) this: Value,
    pub(crate) args: Vec<Value>,
}

pub struct NativeCallable {
    pub(crate) func: Rc<NativeClosure>,
}

#[derive(Clone)]
pub struct UserCallable {
    pub(crate) func: Rc<Function>,
    pub(crate) env: Env,
}

#[derive(Clone)]
pub struct WrappedShadowCallable {
    pub(crate) realm: usize,
    pub(crate) target: Box<Value>,
}

#[derive(Clone)]
pub struct WrappedCrossCallable {
    pub(crate) realm: usize,
    pub(crate) parent: usize,
    pub(crate) target: Box<Value>,
}

impl Callable {
    pub(crate) fn user(func: Rc<Function>, env: Env) -> Callable {
        Callable::User(Rc::new(UserCallable { func, env }))
    }

    pub(crate) fn wrapped_shadow(realm: usize, target: Value) -> Callable {
        Callable::WrappedShadow(Rc::new(WrappedShadowCallable {
            realm,
            target: Box::new(target),
        }))
    }

    pub(crate) fn bound(target: Gc, this: Value, args: Vec<Value>) -> Callable {
        Callable::Bound(Box::new(BoundCallable { target, this, args }))
    }

    pub(crate) fn wrapped_cross(realm: usize, parent: usize, target: Value) -> Callable {
        Callable::WrappedCross(Box::new(WrappedCrossCallable {
            realm,
            parent,
            target: Box::new(target),
        }))
    }
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
    StrWrap(Box<crate::lstr::LStr>),
    SymWrap(Rc<SymbolData>),
    BigIntWrap(Box<crate::bigint::JsBigInt>),
    /// An error object. Carries the captured call-stack frames as a preformatted string (the
    /// `\n    at <fn>` lines, empty when thrown at top level), snapshotted at construction; the
    /// `Error.prototype.stack` getter prepends the live `name: message` head. name/message live as
    /// ordinary properties, and the tag lets `Error.prototype.toString` / the test262 runner
    /// recognise an error cheaply.
    Error(Box<Rc<str>>),
    /// An `arguments` exotic object (mapped index/parameter aliasing lives in
    /// `Interp::mapped_arguments`).
    Arguments,
}

impl Exotic {
    pub(crate) fn str_wrap(value: crate::lstr::LStr) -> Exotic {
        Exotic::StrWrap(Box::new(value))
    }

    pub(crate) fn bigint_wrap(value: crate::bigint::JsBigInt) -> Exotic {
        Exotic::BigIntWrap(Box::new(value))
    }

    pub(crate) fn error(stack: Rc<str>) -> Exotic {
        Exotic::Error(Box::new(stack))
    }
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
    /// GC scratch: mark bit and internal-reference count while collection runs. Between
    /// collections `gc_internal` holds this object's raw-registry slot; the collector restores
    /// every slot before sweeping can drop an object.
    pub(crate) gc_mark: Cell<bool>,
    pub(crate) gc_internal: Cell<u32>,
}

impl Object {
    pub(crate) fn new(proto: Option<Gc>) -> Gc {
        Self::new_with_capacity(proto, 0)
    }

    /// Allocate an ordinary object's named-property vector at its known final size. Constructor
    /// chunks derive a conservative straight-line field count, replacing the usual 1 → 2 → 4
    /// growth sequence with one exact allocation. The hint lives on shared code, not instances.
    pub(crate) fn new_with_capacity(proto: Option<Gc>, property_capacity: usize) -> Gc {
        Self::new_with_parts(proto, Props::with_capacity(property_capacity), Exotic::None)
    }

    /// Allocate an object around an already-finalized property map. Literal fast paths can build
    /// the map from moved stack values before allocation, avoiding an empty map plus RefCell
    /// replacement on every object.
    pub(crate) fn new_with_parts(proto: Option<Gc>, props: Props, exotic: Exotic) -> Gc {
        GC_STATE.with(|state| {
            state.live.set(state.live.get() + 1);
            let mut reg = state.registry.borrow_mut();
            let slot = match reg.free.pop() {
                Some(slot) => slot,
                None => {
                    let slot = reg.entries.len();
                    reg.entries.push(std::ptr::null());
                    slot
                }
            };
            let slot_u32: u32 = slot.try_into().expect("object registry exceeded u32 slots");
            let obj = Rc::new(RefCell::new(Object {
                proto,
                props,
                extensible: true,
                call: Callable::None,
                exotic,
                ic_plain: Cell::new(true),
                is_constructor: false,
                gc_mark: Cell::new(false),
                gc_internal: Cell::new(slot_u32),
            }));
            reg.entries[slot] = Rc::as_ptr(&obj);
            obj
        })
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // Remove the raw registry pointer before the surrounding RcBox is freed. Tombstone reuse
        // is O(1), does not touch another (possibly borrowed) object, and bounds registry memory
        // by peak simultaneously-live objects instead of cumulative allocation count.
        let slot = self.gc_internal.get() as usize;
        let _ = GC_STATE.try_with(|state| {
            let mut reg = state.registry.borrow_mut();
            if slot < reg.entries.len() && !reg.entries[slot].is_null() {
                reg.entries[slot] = std::ptr::null();
                reg.free.push(slot);
            }
            drop(reg);
            state.live.set(state.live.get() - 1);
        });
        // `try_with` so a drop during thread-local teardown at process exit can't panic.
    }
}

// The GC is a refcount-based cycle collector (lumen has no tracing GC). Every heap object is
// registered through a non-owning raw slot and the live count is maintained via Object::new /
// Drop. `Interp::gc_collect` reclaims objects referenced only by other (also-unreachable)
// objects — see interpreter.rs.
struct GcRegistry {
    entries: Vec<*const RefCell<Object>>,
    free: Vec<usize>,
}

struct GcState {
    registry: RefCell<GcRegistry>,
    live: Cell<i64>,
}

thread_local! {
    static GC_STATE: GcState = GcState {
        registry: RefCell::new(GcRegistry {
            entries: Vec::new(),
            free: Vec::new(),
        }),
        live: Cell::new(0),
    };
}

/// Number of live heap objects right now.
pub fn live_objects() -> i64 {
    GC_STATE.with(|state| state.live.get())
}

/// Stable address of this thread's live-object counter. The Rc-based runtime and its compiled
/// chunks are `!Send`, so generated code executes on the thread that baked this TLS address.
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub(crate) fn live_objects_ptr() -> *const i64 {
    GC_STATE.with(|state| state.live.as_ptr())
}

/// Strong handles to every currently-live heap object. Registry slots are non-owning raw
/// pointers tombstoned synchronously by `Object::drop`; while this thread-local borrow is held no
/// object can disappear between reading a slot and incrementing its strong count.
pub fn gc_snapshot() -> Vec<Gc> {
    GC_STATE.with(|state| {
        let reg = state.registry.borrow();
        let mut live = Vec::with_capacity(reg.entries.len() - reg.free.len());
        for &ptr in &reg.entries {
            if ptr.is_null() {
                continue;
            }
            unsafe {
                Rc::increment_strong_count(ptr);
                live.push(Rc::from_raw(ptr));
            }
        }
        live
    })
}

/// Restore `gc_internal` from scratch reference counts to registry-slot ids. Collection calls
/// this after marking and before sweeping side tables/properties can release the final owner of
/// any object, so `Object::drop` always sees its stable slot.
pub(crate) fn gc_restore_registry_slots() {
    GC_STATE.with(|state| {
        let reg = state.registry.borrow();
        for (slot, &ptr) in reg.entries.iter().enumerate() {
            if !ptr.is_null() {
                let slot: u32 = slot.try_into().expect("object registry exceeded u32 slots");
                unsafe { (*ptr).borrow().gc_internal.set(slot) };
            }
        }
    });
}

#[cfg(test)]
pub(crate) fn gc_registry_stats() -> (usize, usize) {
    GC_STATE.with(|state| {
        let reg = state.registry.borrow();
        (reg.entries.len(), reg.free.len())
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
    #[cfg(test)]
    pub(crate) fn write_bigint(self, n: i128) -> Vec<u8> {
        let mut bytes = [0; 8];
        self.write_bigint_into(n, &mut bytes);
        bytes.to_vec()
    }
    /// Encode a BigInt into a fixed-size scratch buffer without allocating.
    pub(crate) fn write_bigint_into(self, n: i128, out: &mut [u8; 8]) {
        debug_assert!(self.is_bigint());
        *out = (n as u64).to_le_bytes();
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
    #[cfg(test)]
    pub(crate) fn write(self, n: f64) -> Vec<u8> {
        let mut bytes = [0; 8];
        let len = self.write_into(n, &mut bytes);
        bytes[..len].to_vec()
    }
    /// Encode a Number into a fixed-size scratch buffer without allocating.
    pub(crate) fn write_into(self, n: f64, out: &mut [u8; 8]) -> usize {
        let int = |n: f64| if n.is_finite() { n.trunc() as i64 } else { 0 };
        match self {
            TaKind::I8 => {
                out[0] = int(n) as i8 as u8;
                1
            }
            TaKind::U8 => {
                out[0] = int(n) as u8;
                1
            }
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
                out[0] = c as u8;
                1
            }
            TaKind::I16 => {
                out[..2].copy_from_slice(&(int(n) as i16).to_le_bytes());
                2
            }
            TaKind::U16 => {
                out[..2].copy_from_slice(&(int(n) as u16).to_le_bytes());
                2
            }
            TaKind::I32 => {
                out[..4].copy_from_slice(&(int(n) as i32).to_le_bytes());
                4
            }
            TaKind::U32 => {
                out[..4].copy_from_slice(&(int(n) as u32).to_le_bytes());
                4
            }
            TaKind::F16 => {
                out[..2].copy_from_slice(&f64_to_f16(n).to_le_bytes());
                2
            }
            TaKind::F32 => {
                out[..4].copy_from_slice(&(n as f32).to_le_bytes());
                4
            }
            TaKind::F64 => {
                *out = n.to_le_bytes();
                8
            }
            TaKind::I64 | TaKind::U64 => {
                self.write_bigint_into(int(n) as i128, out);
                8
            }
        }
    }
}

#[cfg(test)]
mod ta_kind_tests {
    use super::TaKind;

    #[test]
    fn fixed_width_writes_match_allocating_encoders() {
        for kind in [
            TaKind::I8,
            TaKind::U8,
            TaKind::U8Clamped,
            TaKind::I16,
            TaKind::U16,
            TaKind::I32,
            TaKind::U32,
            TaKind::F16,
            TaKind::F32,
            TaKind::F64,
            TaKind::I64,
            TaKind::U64,
        ] {
            let mut bytes = [0; 8];
            let len = kind.write_into(-12.75, &mut bytes);
            assert_eq!(&bytes[..len], kind.write(-12.75).as_slice());
        }
    }

    #[test]
    fn bigint_fixed_width_writes_match_allocating_encoder() {
        for kind in [TaKind::I64, TaKind::U64] {
            let mut bytes = [0; 8];
            kind.write_bigint_into(-123_i128, &mut bytes);
            assert_eq!(&bytes, kind.write_bigint(-123).as_slice());
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
/// getter/setter pair. The low bits of `meta` hold the four descriptor flags; its aligned upper
/// bits point to an accessor pair only for accessor properties. Thus ordinary properties are 32
/// bytes and allocate no metadata, while still keeping the flags directly readable by the JIT.
pub struct Property {
    packed: PackedValue,
    meta: usize,
}

/// The boxed getter/setter pair of an accessor property.
#[repr(align(16))]
#[derive(Clone, Default)]
pub(crate) struct Accessors {
    pub get: Option<Value>,
    pub set: Option<Value>,
}

pub(crate) const PROP_ACCESSOR: usize = 1;
pub(crate) const PROP_WRITABLE: usize = 2;
pub(crate) const PROP_ENUMERABLE: usize = 4;
pub(crate) const PROP_CONFIGURABLE: usize = 8;
const PROP_FLAG_MASK: usize = 15;

impl Clone for Property {
    fn clone(&self) -> Self {
        let flags = self.meta & PROP_FLAG_MASK;
        let ptr = self.meta & !PROP_FLAG_MASK;
        let meta = if ptr == 0 {
            flags
        } else {
            let acc = unsafe { &*(ptr as *const Accessors) };
            Box::into_raw(Box::new(acc.clone())) as usize | flags
        };
        Property {
            packed: self.packed.clone(),
            meta,
        }
    }
}

impl Drop for Property {
    fn drop(&mut self) {
        let ptr = self.meta & !PROP_FLAG_MASK;
        if ptr != 0 {
            unsafe { drop(Box::from_raw(ptr as *mut Accessors)) };
        }
    }
}

impl Property {
    pub(crate) fn data(
        value: Value,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        let meta = (writable as usize) * PROP_WRITABLE
            | (enumerable as usize) * PROP_ENUMERABLE
            | (configurable as usize) * PROP_CONFIGURABLE;
        Property {
            packed: PackedValue::pack(value),
            meta,
        }
    }
    /// An accessor property (`accessor: true`, value `Undefined`, not writable).
    pub(crate) fn accessor_prop(
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        let flags = PROP_ACCESSOR
            | (enumerable as usize) * PROP_ENUMERABLE
            | (configurable as usize) * PROP_CONFIGURABLE;
        let ptr = Box::into_raw(Box::new(Accessors { get, set })) as usize;
        debug_assert_eq!(ptr & PROP_FLAG_MASK, 0);
        Property {
            packed: PackedValue::pack(Value::Undefined),
            meta: ptr | flags,
        }
    }
    #[inline]
    fn accessors(&self) -> Option<&Accessors> {
        let ptr = self.meta & !PROP_FLAG_MASK;
        (ptr != 0).then(|| unsafe { &*(ptr as *const Accessors) })
    }
    #[inline]
    fn accessors_mut(&mut self) -> Option<&mut Accessors> {
        let ptr = self.meta & !PROP_FLAG_MASK;
        (ptr != 0).then(|| unsafe { &mut *(ptr as *mut Accessors) })
    }
    #[inline]
    pub(crate) fn accessor(&self) -> bool {
        self.meta & PROP_ACCESSOR != 0
    }
    #[inline]
    pub(crate) fn writable(&self) -> bool {
        self.meta & PROP_WRITABLE != 0
    }
    #[inline]
    pub(crate) fn enumerable(&self) -> bool {
        self.meta & PROP_ENUMERABLE != 0
    }
    #[inline]
    pub(crate) fn configurable(&self) -> bool {
        self.meta & PROP_CONFIGURABLE != 0
    }
    fn set_flag(&mut self, flag: usize, value: bool) {
        if value {
            self.meta |= flag;
        } else {
            self.meta &= !flag;
        }
    }
    pub(crate) fn set_accessor(&mut self, value: bool) {
        if !value {
            self.clear_accessors();
        }
        self.set_flag(PROP_ACCESSOR, value);
    }
    pub(crate) fn set_writable(&mut self, value: bool) {
        self.set_flag(PROP_WRITABLE, value);
    }
    pub(crate) fn set_enumerable(&mut self, value: bool) {
        self.set_flag(PROP_ENUMERABLE, value);
    }
    pub(crate) fn set_configurable(&mut self, value: bool) {
        self.set_flag(PROP_CONFIGURABLE, value);
    }
    pub(crate) fn into_value(mut self) -> Value {
        self.take_value()
    }
    #[inline]
    pub(crate) fn value(&self) -> Value {
        self.packed.unpack()
    }
    #[inline]
    pub(crate) fn set_value(&mut self, value: Value) {
        self.packed = PackedValue::pack(value);
    }
    #[inline]
    pub(crate) fn replace_value(&mut self, value: Value) -> Value {
        let old = std::mem::replace(&mut self.packed, PackedValue::pack(value));
        old.into_value()
    }
    #[inline]
    pub(crate) fn take_value(&mut self) -> Value {
        self.replace_value(Value::Undefined)
    }
    #[inline]
    pub(crate) fn getter(&self) -> Option<&Value> {
        self.accessors().and_then(|a| a.get.as_ref())
    }
    #[inline]
    pub(crate) fn setter(&self) -> Option<&Value> {
        self.accessors().and_then(|a| a.set.as_ref())
    }
    pub(crate) fn set_getter(&mut self, g: Option<Value>) {
        if let Some(a) = self.accessors_mut() {
            a.get = g;
        } else if let Some(g) = g {
            let flags = self.meta & PROP_FLAG_MASK;
            let ptr = Box::into_raw(Box::new(Accessors {
                get: Some(g),
                set: None,
            })) as usize;
            self.meta = ptr | flags;
        }
    }
    pub(crate) fn set_setter(&mut self, s: Option<Value>) {
        if let Some(a) = self.accessors_mut() {
            a.set = s;
        } else if let Some(s) = s {
            let flags = self.meta & PROP_FLAG_MASK;
            let ptr = Box::into_raw(Box::new(Accessors {
                get: None,
                set: Some(s),
            })) as usize;
            self.meta = ptr | flags;
        }
    }
    /// Drop the accessor pair (used when a define converts an accessor back to a data property).
    pub(crate) fn clear_accessors(&mut self) {
        let ptr = self.meta & !PROP_FLAG_MASK;
        if ptr != 0 {
            unsafe { drop(Box::from_raw(ptr as *mut Accessors)) };
            self.meta &= PROP_FLAG_MASK;
        }
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

pub(crate) mod gc_edges;
mod props;
pub use props::Props;
pub(crate) use props::{
    bump_proto_epoch, fn_key, index_key, proto_epoch, proto_epoch_ptr, MIRROR_ALL_I32,
    MIRROR_NO_HOLES, MIRROR_OK,
};
use props::{DenseBuffers, DenseStorage};

/// A canonical array-index property key (`"0"`, `"42"` — decimal, no leading zeros, fits u32).
#[inline(always)]
pub(crate) fn canonical_index(k: &str) -> Option<u32> {
    let bytes = k.as_bytes();
    let &first = bytes.first()?;
    // Named properties dominate. Reject them after one byte instead of setting up the full
    // iterator/parser path; this function sits in every generic property lookup.
    if !first.is_ascii_digit() {
        return None;
    }
    if first == b'0' {
        return (bytes.len() == 1).then_some(0);
    }
    if !bytes[1..].iter().all(u8::is_ascii_digit) {
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
